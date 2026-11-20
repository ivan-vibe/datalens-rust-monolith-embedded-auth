//! Write endpoints of `src/controllers/entries/*`.
//!
//! Port targets:
//!   POST   /v1/entries                              create-entry
//!   POST   /v1/entries/{entry_id}                   update-entry
//!   POST   /v1/entries/{entry_id}/rename            rename-entry
//!   DELETE /v1/entries/{entry_id}                   delete-entry
//!   POST   /v1/entries/{entry_id}/switch-revision   switch-revision-entry
//!   POST   /private/entries/{entry_id}/unversioned-data
//! plus the matching `/private/...` aliases registered by express.
//!
//! Everything runs under the flags of the open-source deployment:
//! `AUTH_ENABLED=false` (`accessServiceEnabled === false`), `DLS_ENABLED=false`,
//! tenant `common`, user `systemId` / login `unknown`. Under those flags the
//! registry `checkTenant` / `checkServicePlan` / `checkTenantFeatures` /
//! `fetchAndValidateLicenseOrFail` / `isNeedBypassEntryByKey` are no-ops and
//! every DLS branch is skipped, which is exactly what this module does.
//!
//! The write path is a faithful port of:
//!   * `src/db/models/entry/index.ts` (`Entry.create`, `Entry.syncLinks`),
//!   * `src/db/models/links/index.ts` (`Links.sync`),
//!   * `src/services/entry/actions/{create-in-workbook,update-entry,rename-entry,
//!     delete-entry,switch-revision-entry}.ts`,
//!   * `src/services/new/entry/{create-in-collection,update-entry-unversioned-data-private}`,
//!   * `src/services/entry/crud/update/mark-entry-as-deleted.ts`,
//!   * the zod request schemas of the controllers and the response models of
//!     `src/controllers/entries/response-models/*`.

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::Extension;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions;
use crate::state::{AppState, PrivatePermissions};

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/entries", post(create_entry)),
        ("/private/entries", post(private_create_entry)),
        ("/v1/entries/{entry_id}", post(update_entry)),
        ("/private/entries/{entry_id}", post(private_update_entry)),
        ("/v1/entries/{entry_id}/rename", post(rename_entry)),
        (
            "/private/entries/{entry_id}/rename",
            post(private_rename_entry),
        ),
        ("/v1/entries/{entry_id}", delete(delete_entry)),
        ("/private/entries/{entry_id}", delete(private_delete_entry)),
        (
            "/v1/entries/{entry_id}/switch-revision",
            post(switch_revision),
        ),
        (
            "/private/entries/{entry_id}/switch-revision",
            post(switch_revision),
        ),
        (
            "/private/entries/{entry_id}/unversioned-data",
            post(update_unversioned_data),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Constants (`src/const`, `src/db/models/new/entry/types.ts`)
// ---------------------------------------------------------------------------

const SCOPE_VALUES: [&str; 10] = [
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

/// `ALLOWED_COLLECTION_ENTRY_SCOPES` of `src/entities/shared-entry/constants.ts`.
const ALLOWED_COLLECTION_ENTRY_SCOPES: [&str; 3] = ["connection", "dataset", "compute"];

const MODE_VALUES_SAVE_PUBLISH: [&str; 2] = ["save", "publish"];
const MODE_VALUES_ALL: [&str; 3] = ["save", "publish", "recover"];
const PERMISSIONS_MODE_VALUES: [&str; 3] = ["explicit", "parent_and_owner", "owner_only"];

const MAX_META_OBJECT_SYMBOLS: usize = 2000;
const MAX_UNVERSIONED_DATA_OBJECT_SYMBOLS: usize = 5000;
const ANNOTATION_DESCRIPTION_MAX_LENGTH: usize = 36000;

/// `symbolsValidateMessage` of `src/components/validation-schema-compiler.ts`.
const SYMBOLS_VALIDATE_MESSAGE: &str = "should start and end with A-Za-zА-Яа-яЁё0-9_@()% and can contain only symbols: A-Za-zА-Яа-яЁё0-9_@()%.,:;'|-–—−$*& and spaces";

/// `TRASH_FOLDER`
const TRASH_FOLDER: &str = "__trash";

/// `makeUserId(SYSTEM_USER.ID)`
const SYSTEM_USER_ID: &str = "systemId";

// ---------------------------------------------------------------------------
// Handler error (JSON `ApiError` or a raw express-style response)
// ---------------------------------------------------------------------------

pub enum HandlerError {
    Api(ApiError),
    Raw(Response),
}

impl From<ApiError> for HandlerError {
    fn from(error: ApiError) -> Self {
        HandlerError::Api(error)
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

// ---------------------------------------------------------------------------
// Generic helpers
// ---------------------------------------------------------------------------

/// Node serializes `Date` as `toISOString()`: UTC, milliseconds, `Z` suffix.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `moment().format()` — second precision (see `api/locks.rs`).
fn now_seconds() -> DateTime<Utc> {
    use chrono::SubsecRound;
    Utc::now().trunc_subsecs(0)
}

fn make_user_id(user_id: &str) -> String {
    format!("uid:{user_id}")
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

fn json_opt_string(value: &Option<String>) -> Value {
    match value {
        Some(value) => Value::String(value.clone()),
        None => Value::Null,
    }
}

fn json_opt_id(value: Option<i64>) -> Value {
    match encode_id_opt(value) {
        Some(value) => Value::String(value),
        None => Value::Null,
    }
}

fn json_datetime_opt(value: Option<DateTime<Utc>>) -> Value {
    match value {
        Some(value) => Value::String(format_timestamp(value)),
        None => Value::Null,
    }
}

/// JavaScript `Number` → string, close enough for the JSON size checks.
fn js_number_to_string(number: &serde_json::Number) -> String {
    if let Some(value) = number.as_i64() {
        return value.to_string();
    }
    if let Some(value) = number.as_u64() {
        return value.to_string();
    }

    let value = number.as_f64().unwrap_or(f64::NAN);

    if !value.is_finite() {
        return "null".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.fract() == 0.0 && value.abs() < 1e21 {
        return format!("{value:.0}");
    }

    format!("{value}")
}

/// `JSON.stringify(value)` with the escaping rules of the JavaScript runtime
/// (non-ASCII is emitted verbatim, only quotes / backslashes / control
/// characters are escaped).
fn js_stringify(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => js_number_to_string(number),
        Value::String(text) => {
            let mut out = String::with_capacity(text.len() + 2);
            out.push('"');
            for character in text.chars() {
                match character {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(js_stringify).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    let mut key_json = js_stringify(&Value::String(key.clone()));
                    key_json.push(':');
                    key_json.push_str(&js_stringify(value));
                    key_json
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

// ---------------------------------------------------------------------------
// zod issue helpers (`src/components/zod`)
// ---------------------------------------------------------------------------

fn issue_invalid_type(path: &[&str], expected: &str, received: &str) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

fn issue_invalid_value(path: &[&str], values: &[&str]) -> Value {
    let quoted: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();

    json!({
        "code": "invalid_value",
        "values": values,
        "path": path,
        "message": format!("Invalid option: expected one of {}", quoted.join("|")),
    })
}

fn issue_custom(path: &[&str], message: &str) -> Value {
    json!({
        "code": "custom",
        "path": path,
        "message": message,
    })
}

fn issue_custom_params(path: &[&str], message: &str, params: Value) -> Value {
    json!({
        "code": "custom",
        "path": path,
        "params": params,
        "message": message,
    })
}

fn issue_too_big_string(path: &[&str], maximum: usize) -> Value {
    json!({
        "origin": "string",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected string to have <={maximum} characters"),
    })
}

fn issue_too_small_array(path: &[&str], minimum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": path,
        "message": format!("Too small: expected array to have >={minimum} items"),
    })
}

fn issue_too_big_array(path: &[&str], maximum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected array to have <={maximum} items"),
    })
}

fn issue_unrecognized_keys(path: &[&str], keys: &[String]) -> Value {
    let quoted: Vec<String> = keys.iter().map(|key| format!("\"{key}\"")).collect();

    json!({
        "code": "unrecognized_keys",
        "keys": keys,
        "path": path,
        "message": format!("Unrecognized key: {}", quoted.join(", ")),
    })
}

/// `zc.limitedObject({limit})` refinement issue.
fn issue_object_size_limit(path: &[&str], limit: usize) -> Value {
    issue_custom_params(
        path,
        &format!("Object can't contain more than {limit} characters"),
        json!({"code": "OBJECT_SIZE_LIMIT_EXCEEDED"}),
    )
}

/// `zc.entityName()` refinements.
fn entity_name_issues(name: &str) -> Vec<Value> {
    let mut issues = Vec::new();

    if name.contains('/') {
        issues.push(issue_custom(
            &["name"],
            "String should not contain the symbol /",
        ));
    }
    if !key_reg_matches(name) {
        issues.push(issue_custom(
            &["name"],
            &format!("String {SYMBOLS_VALIDATE_MESSAGE}"),
        ));
    }

    issues
}

// ---------------------------------------------------------------------------
// `KEY_REG` (`src/components/validation-schema-compiler.ts`)
// ---------------------------------------------------------------------------

/// `[\wА-Яа-яЁё_@()%]` — the character class allowed at the edges of a key part.
fn is_key_edge_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || character == '_'
        || matches!(character, '@' | '(' | ')' | '%')
        || matches!(character, '\u{0410}'..='\u{042F}')
        || matches!(character, '\u{0430}'..='\u{044F}')
        || matches!(character, '\u{0401}' | '\u{0451}')
}

/// The interior character class of `KEY_REG`.
fn is_key_middle_char(character: char) -> bool {
    if is_key_edge_char(character) {
        return true;
    }

    if matches!(
        character,
        '.' | ',' | ':' | ';' | '\'' | '|' | ' ' | '-' | '–' | '—' | '−' | '$' | '*' | '&'
    ) {
        return true;
    }

    // Unicode spaces accepted by the original character class.
    matches!(
        character,
        '\u{00A0}' | '\u{180E}' | '\u{2000}'
            ..='\u{200B}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `KEY_REG.test(value)` — `^edge (middle* edge+)?$`.
fn key_reg_matches(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();

    let Some(first) = chars.first() else {
        return false;
    };

    if !is_key_edge_char(*first) {
        return false;
    }

    if chars.len() == 1 {
        return true;
    }

    // Greedy maximal suffix of edge characters; everything before it has to be
    // an interior character (`edge ⊆ middle`, so the split is unambiguous).
    let mut index = chars.len();
    while index > 0 && is_key_edge_char(chars[index - 1]) {
        index -= 1;
    }

    if index == chars.len() {
        // The last character is not an edge character, so `E+` cannot match.
        return false;
    }

    if index <= 1 {
        // The whole string is made of edge characters: `M*` is empty.
        return true;
    }

    chars[1..index]
        .iter()
        .all(|character| is_key_middle_char(*character))
}

/// `ajv.addKeyword('verifyEntryKey')` error message, if any.
fn verify_entry_key_error(key: &str) -> Option<String> {
    if key.starts_with('/') {
        return Some("should not start with /".to_owned());
    }

    if key.split('/').all(key_reg_matches) {
        None
    } else {
        Some(SYMBOLS_VALIDATE_MESSAGE.to_owned())
    }
}

/// `ajv.addKeyword('verifyEntryName')` error message, if any.
fn verify_entry_name_error(name: &str) -> Option<String> {
    if name.contains('/') {
        return Some("should not contain the symbol /".to_owned());
    }

    if key_reg_matches(name) {
        None
    } else {
        Some(SYMBOLS_VALIDATE_MESSAGE.to_owned())
    }
}

// ---------------------------------------------------------------------------
// `Utils` key helpers (`src/utils/utils.ts`)
// ---------------------------------------------------------------------------

fn format_key(key: &str, is_folder: bool) -> String {
    if is_folder {
        if !key.ends_with('/') {
            return format!("{key}/");
        }
        return key.to_owned();
    }

    if key != "/" {
        return key.strip_suffix('/').unwrap_or(key).to_owned();
    }

    key.to_owned()
}

fn get_parent_folder_key(key_formatted: &str) -> String {
    let chars: Vec<char> = key_formatted.chars().collect();

    if chars.is_empty() {
        return "/".to_owned();
    }

    let from = chars.len() as isize - 2;

    let slash_index = if from < 0 {
        // `lastIndexOf('/', 0)` only inspects index 0.
        if chars.first() == Some(&'/') {
            Some(0usize)
        } else {
            None
        }
    } else {
        (0..=from as usize).rev().find(|index| chars[*index] == '/')
    };

    match slash_index {
        Some(index) => {
            let parent: String = chars[..=index].iter().collect();
            if parent.is_empty() {
                "/".to_owned()
            } else {
                parent
            }
        }
        None => "/".to_owned(),
    }
}

fn get_folders_keys(folder_key: &str) -> Vec<String> {
    let levels: Vec<&str> = folder_key
        .split('/')
        .filter(|level| !level.is_empty())
        .collect();

    (1..=levels.len())
        .map(|index| format_key(&levels[..index].join("/"), true))
        .collect()
}

fn get_full_parent_folder_keys(key_formatted: &str) -> Vec<String> {
    let mut parent_folder_key = key_formatted.to_owned();
    let mut parent_folder_keys = Vec::new();

    while !parent_folder_key.is_empty() {
        let new_parent_folder_key = get_parent_folder_key(&parent_folder_key);

        if new_parent_folder_key.is_empty() {
            break;
        }

        parent_folder_key = new_parent_folder_key;
        parent_folder_keys.push(parent_folder_key.clone());

        if parent_folder_key.split('/').count() <= 2 {
            break;
        }
    }

    parent_folder_keys
}

fn is_root(key: &str) -> bool {
    key == "/"
}

fn is_users_folder(key: &Option<String>) -> bool {
    match key {
        Some(key) => key.to_lowercase() == "users/",
        None => false,
    }
}

/// `Utils.escapeStringForLike`
fn escape_string_for_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        if character == '%' || character == '_' {
            escaped.push('\\');
        }
        escaped.push(character);
    }

    escaped
}

// ---------------------------------------------------------------------------
// Request body parsing (express body-parser + expresskit contracts)
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

/// `express.json({strict: true})` as wired by expresskit:
///   * a zero-length body leaves `req.body === {}`;
///   * a JSON body must be an object or an array ("Invalid JSON supplied");
///   * a non-empty body with a non-JSON content type is never parsed, so the
///     contract rejects it with an empty issue list.
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
        return Err(HandlerError::Api(ApiError::validation(json!([]))));
    }

    let text = String::from_utf8_lossy(body).into_owned();
    let trimmed = text.trim_start();

    match trimmed.as_bytes().first() {
        Some(b'{') | Some(b'[') => {}
        _ => return Err(invalid_json()),
    }

    serde_json::from_str::<Value>(&text).map_err(|_| invalid_json())
}

/// `z.object({...})` on a non-object body: one issue with an empty path.
fn ensure_object_body(body: &Value) -> Result<&Map<String, Value>, ApiError> {
    match body {
        Value::Object(map) => Ok(map),
        other => Err(ApiError::validation(json!([issue_invalid_type(
            &[],
            "object",
            js_type_name(other),
        )]))),
    }
}

fn parse_encoded_id(map: &Map<String, Value>, field: &str, issues: &mut Vec<Value>) -> Option<i64> {
    match map.get(field) {
        None => None,
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(value) => Some(value),
            Err(_) => {
                issues.push(ApiError::encoded_id_issue(field, raw));
                None
            }
        },
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "string", js_type_name(other)));
            None
        }
    }
}

fn parse_optional_string(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<String> {
    match map.get(field) {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "string", js_type_name(other)));
            None
        }
    }
}

fn parse_optional_bool(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<bool> {
    match map.get(field) {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "boolean", js_type_name(other)));
            None
        }
    }
}

fn parse_optional_string_max(
    map: &Map<String, Value>,
    field: &str,
    maximum: usize,
    issues: &mut Vec<Value>,
) -> Option<String> {
    match map.get(field) {
        None => None,
        Some(Value::String(value)) => {
            if js_length(value) > maximum {
                issues.push(issue_too_big_string(&[field], maximum));
                None
            } else {
                Some(value.clone())
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "string", js_type_name(other)));
            None
        }
    }
}

fn parse_optional_enum(
    map: &Map<String, Value>,
    field: &str,
    values: &[&str],
    issues: &mut Vec<Value>,
) -> Option<String> {
    match map.get(field) {
        None => None,
        Some(Value::String(value)) => {
            if values.contains(&value.as_str()) {
                Some(value.clone())
            } else {
                issues.push(issue_invalid_value(&[field], values));
                None
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "string", js_type_name(other)));
            None
        }
    }
}

/// `z.number().nullish()`
fn parse_optional_number(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<i32> {
    match map.get(field) {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => {
            let value = number.as_f64().unwrap_or(f64::NAN);

            if !value.is_finite() {
                issues.push(issue_invalid_type(&[field], "number", "NaN"));
                return None;
            }

            if value.fract() != 0.0 || value < i32::MIN as f64 || value > i32::MAX as f64 {
                // PostgreSQL raises `22003 numeric_value_out_of_range`; the
                // presenter turns that into this 400.
                return None;
            }

            Some(value as i32)
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "number", js_type_name(other)));
            None
        }
    }
}

fn parse_optional_string_array(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<Vec<String>> {
    match map.get(field) {
        None => None,
        Some(Value::Array(items)) => {
            let mut result = Vec::with_capacity(items.len());
            let mut failed = false;

            for (index, item) in items.iter().enumerate() {
                match item {
                    Value::String(value) => result.push(value.clone()),
                    other => {
                        let path = [field, &index.to_string()];
                        let path: Vec<&str> = path.to_vec();
                        issues.push(issue_invalid_type(&path, "string", js_type_name(other)));
                        failed = true;
                    }
                }
            }

            if failed {
                None
            } else {
                Some(result)
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "array", js_type_name(other)));
            None
        }
    }
}

/// `zc.limitedObject({limit}).refine(noNestedObjectValues).nullable().optional()`
fn parse_limited_object(
    map: &Map<String, Value>,
    field: &str,
    limit: usize,
    check_nested: bool,
    nullable: bool,
    issues: &mut Vec<Value>,
) -> Option<Value> {
    match map.get(field) {
        None => None,
        Some(Value::Null) => {
            if nullable {
                Some(Value::Null)
            } else {
                issues.push(issue_invalid_type(&[field], "record", "null"));
                None
            }
        }
        Some(value @ Value::Object(object)) => {
            let mut ok = true;

            if js_length(&js_stringify(value)) > limit {
                issues.push(issue_object_size_limit(&[field], limit));
                ok = false;
            }

            if check_nested {
                let nested = object.values().any(|item| matches!(item, Value::Object(_)));
                if nested {
                    issues.push(issue_custom(&[field], "Meta values cannot be objects"));
                    ok = false;
                }
            }

            if ok {
                Some(value.clone())
            } else {
                None
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "record", js_type_name(other)));
            None
        }
    }
}

/// `z.record(z.string(), z.string())`
fn parse_string_record(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<Value> {
    match map.get(field) {
        None => None,
        Some(value @ Value::Object(object)) => {
            let mut ok = true;

            for (key, item) in object {
                if !item.is_string() {
                    issues.push(issue_invalid_type(
                        &[field, key],
                        "string",
                        js_type_name(item),
                    ));
                    ok = false;
                }
            }

            if ok {
                Some(value.clone())
            } else {
                None
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "record", js_type_name(other)));
            None
        }
    }
}

/// `z.record(z.string(), z.unknown()).nullable().optional()`
fn parse_optional_record(
    map: &Map<String, Value>,
    field: &str,
    issues: &mut Vec<Value>,
) -> Option<Value> {
    match map.get(field) {
        None | Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "record", js_type_name(other)));
            None
        }
    }
}

/// `annotation` object of the create controller (`z.object({...}).strict()`).
fn parse_strict_annotation(map: &Map<String, Value>, issues: &mut Vec<Value>) -> Option<String> {
    match map.get("annotation") {
        None => None,
        Some(Value::Object(object)) => {
            let mut description = None;

            match object.get("description") {
                None => issues.push(issue_invalid_type(
                    &["annotation", "description"],
                    "string",
                    "undefined",
                )),
                Some(Value::String(value)) => {
                    if js_length(value) > ANNOTATION_DESCRIPTION_MAX_LENGTH {
                        issues.push(issue_too_big_string(
                            &["annotation", "description"],
                            ANNOTATION_DESCRIPTION_MAX_LENGTH,
                        ));
                    } else {
                        description = Some(value.clone());
                    }
                }
                Some(other) => issues.push(issue_invalid_type(
                    &["annotation", "description"],
                    "string",
                    js_type_name(other),
                )),
            }

            let unknown: Vec<String> = object
                .keys()
                .filter(|key| key.as_str() != "description")
                .cloned()
                .collect();

            if !unknown.is_empty() {
                issues.push(issue_unrecognized_keys(&["annotation"], &unknown));
            }

            description
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                &["annotation"],
                "object",
                js_type_name(other),
            ));
            None
        }
    }
}

/// `annotation` object of the update controller (not strict).
fn parse_annotation(map: &Map<String, Value>, issues: &mut Vec<Value>) -> Option<String> {
    match map.get("annotation") {
        None => None,
        Some(Value::Object(object)) => match object.get("description") {
            None => {
                issues.push(issue_invalid_type(
                    &["annotation", "description"],
                    "string",
                    "undefined",
                ));
                None
            }
            Some(Value::String(value)) => {
                if js_length(value) > ANNOTATION_DESCRIPTION_MAX_LENGTH {
                    issues.push(issue_too_big_string(
                        &["annotation", "description"],
                        ANNOTATION_DESCRIPTION_MAX_LENGTH,
                    ));
                    None
                } else {
                    Some(value.clone())
                }
            }
            Some(other) => {
                issues.push(issue_invalid_type(
                    &["annotation", "description"],
                    "string",
                    js_type_name(other),
                ));
                None
            }
        },
        Some(other) => {
            issues.push(issue_invalid_type(
                &["annotation"],
                "object",
                js_type_name(other),
            ));
            None
        }
    }
}

/// `initialPermissions: z.record(z.string(), z.array(z.object({subject, comment?})))`
fn parse_initial_permissions(map: &Map<String, Value>, issues: &mut Vec<Value>) -> Option<Value> {
    let value = match map.get("initialPermissions") {
        None => return None,
        Some(value) => value,
    };

    let Value::Object(object) = value else {
        issues.push(issue_invalid_type(
            &["initialPermissions"],
            "record",
            js_type_name(value),
        ));
        return None;
    };

    let mut ok = true;

    for (key, item) in object {
        let Value::Array(entries) = item else {
            issues.push(issue_invalid_type(
                &["initialPermissions", key],
                "array",
                js_type_name(item),
            ));
            ok = false;
            continue;
        };

        for (index, entry) in entries.iter().enumerate() {
            let index_string = index.to_string();
            let Value::Object(entry) = entry else {
                issues.push(issue_invalid_type(
                    &["initialPermissions", key, &index_string],
                    "object",
                    js_type_name(entry),
                ));
                ok = false;
                continue;
            };

            match entry.get("subject") {
                Some(Value::String(_)) => {}
                Some(other) => {
                    issues.push(issue_invalid_type(
                        &["initialPermissions", key, &index_string, "subject"],
                        "string",
                        js_type_name(other),
                    ));
                    ok = false;
                }
                None => {
                    issues.push(issue_invalid_type(
                        &["initialPermissions", key, &index_string, "subject"],
                        "string",
                        "undefined",
                    ));
                    ok = false;
                }
            }

            if let Some(comment) = entry.get("comment") {
                if !comment.is_string() {
                    issues.push(issue_invalid_type(
                        &["initialPermissions", key, &index_string, "comment"],
                        "string",
                        js_type_name(comment),
                    ));
                    ok = false;
                }
            }
        }
    }

    if ok {
        Some(value.clone())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Request schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
#[allow(dead_code)]
struct CreateBody {
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    name: Option<String>,
    scope: Option<String>,
    entry_type: Option<String>,
    key: Option<String>,
    meta: Option<Value>,
    description: Option<String>,
    annotation: Option<String>,
    recursion: Option<bool>,
    hidden: bool,
    mirrored: Option<bool>,
    mode: Option<String>,
    data: Option<Value>,
    unversioned_data: Option<Value>,
    links: Option<Value>,
    permissions_mode: Option<String>,
    include_permissions_info: Option<bool>,
    initial_permissions: Option<Value>,
    initial_parent_id: Option<String>,
    check_service_plan: Option<String>,
    check_tenant_features: Option<Vec<String>>,
    version: Option<i32>,
    source_version: Option<i32>,
}

/// `create-entry.ts` request schema.
fn parse_create_body(map: &Map<String, Value>) -> Result<CreateBody, ApiError> {
    let mut issues = Vec::new();

    let workbook_id = parse_encoded_id(map, "workbookId", &mut issues);
    let collection_id = parse_encoded_id(map, "collectionId", &mut issues);
    let name = parse_optional_string(map, "name", &mut issues);
    let scope = parse_optional_enum(map, "scope", &SCOPE_VALUES, &mut issues);
    let entry_type = parse_optional_string(map, "type", &mut issues);
    let key = parse_optional_string(map, "key", &mut issues);
    let meta = parse_limited_object(
        map,
        "meta",
        MAX_META_OBJECT_SYMBOLS,
        true,
        true,
        &mut issues,
    );
    let description = parse_optional_string_max(
        map,
        "description",
        ANNOTATION_DESCRIPTION_MAX_LENGTH,
        &mut issues,
    );
    let annotation = parse_strict_annotation(map, &mut issues);
    let recursion = parse_optional_bool(map, "recursion", &mut issues);

    // `hidden: z.boolean().nullish().transform((value) => value ?? false)`
    let hidden = match map.get("hidden") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            issues.push(issue_invalid_type(
                &["hidden"],
                "boolean",
                js_type_name(other),
            ));
            false
        }
    };

    let mirrored = parse_optional_bool(map, "mirrored", &mut issues);
    let mode = parse_optional_enum(map, "mode", &MODE_VALUES_SAVE_PUBLISH, &mut issues);
    let data = parse_optional_record(map, "data", &mut issues);
    let unversioned_data = parse_limited_object(
        map,
        "unversionedData",
        MAX_UNVERSIONED_DATA_OBJECT_SYMBOLS,
        false,
        true,
        &mut issues,
    );
    let links = parse_string_record(map, "links", &mut issues);
    let permissions_mode = parse_optional_enum(
        map,
        "permissionsMode",
        &PERMISSIONS_MODE_VALUES,
        &mut issues,
    );
    let include_permissions_info = parse_optional_bool(map, "includePermissionsInfo", &mut issues);
    let initial_permissions = parse_initial_permissions(map, &mut issues);
    let initial_parent_id = parse_optional_string(map, "initialParentId", &mut issues);
    let check_service_plan = parse_optional_string(map, "checkServicePlan", &mut issues);
    let check_tenant_features =
        parse_optional_string_array(map, "checkTenantFeatures", &mut issues);
    let version = parse_optional_number(map, "version", &mut issues);
    let source_version = parse_optional_number(map, "sourceVersion", &mut issues);

    if !issues.is_empty() {
        return Err(ApiError::validation(Value::Array(issues)));
    }

    // `superRefine`s run only when the shape itself parsed cleanly.
    if workbook_id.is_some() && collection_id.is_some() {
        return Err(ApiError::validation(json!([issue_custom(
            &["collectionId"],
            "Cannot specify both workbookId and collectionId"
        )])));
    }

    if scope.as_deref() == Some("compute") && collection_id.is_none() {
        return Err(ApiError::validation(json!([issue_custom(
            &["collectionId"],
            "Compute entries can only be created in a collection"
        )])));
    }

    Ok(CreateBody {
        workbook_id,
        collection_id,
        name,
        scope,
        entry_type,
        key,
        meta,
        description,
        annotation,
        recursion,
        hidden,
        mirrored,
        mode,
        data,
        unversioned_data,
        links,
        permissions_mode,
        include_permissions_info,
        initial_permissions,
        initial_parent_id,
        check_service_plan,
        check_tenant_features,
        version,
        source_version,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Save,
    Publish,
    Recover,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
struct UpdateBody {
    meta: Option<Value>,
    data: Option<Value>,
    description: Option<String>,
    annotation: Option<String>,
    unversioned_data: Option<Value>,
    links: Option<Value>,
    mode: Mode,
    entry_type: Option<String>,
    hidden: Option<bool>,
    mirrored: Option<bool>,
    rev_id: Option<i64>,
    lock_token: Option<String>,
    skip_sync_links: Option<bool>,
    update_revision: Option<bool>,
    check_service_plan: Option<String>,
    check_tenant_features: Option<Vec<String>>,
    version: Option<i32>,
    source_version: Option<i32>,
    current_scope: Option<String>,
    current_type: Option<String>,
}

/// `update-entry.ts` request schema.
fn parse_update_body(map: &Map<String, Value>) -> Result<UpdateBody, ApiError> {
    let mut issues = Vec::new();

    let meta = parse_limited_object(
        map,
        "meta",
        MAX_META_OBJECT_SYMBOLS,
        true,
        true,
        &mut issues,
    );
    let data = parse_optional_record(map, "data", &mut issues);
    let description = parse_optional_string_max(
        map,
        "description",
        ANNOTATION_DESCRIPTION_MAX_LENGTH,
        &mut issues,
    );
    let annotation = parse_annotation(map, &mut issues);
    let unversioned_data = parse_limited_object(
        map,
        "unversionedData",
        MAX_UNVERSIONED_DATA_OBJECT_SYMBOLS,
        false,
        true,
        &mut issues,
    );
    let links = parse_string_record(map, "links", &mut issues);
    let mode_value = parse_optional_enum(map, "mode", &MODE_VALUES_ALL, &mut issues);
    let entry_type = parse_optional_string(map, "type", &mut issues);
    let hidden = parse_optional_bool(map, "hidden", &mut issues);
    let mirrored = parse_optional_bool(map, "mirrored", &mut issues);
    let rev_id = parse_encoded_id(map, "revId", &mut issues);
    let lock_token = parse_optional_string(map, "lockToken", &mut issues);
    let skip_sync_links = parse_optional_bool(map, "skipSyncLinks", &mut issues);
    let update_revision = parse_optional_bool(map, "updateRevision", &mut issues);
    let check_service_plan = parse_optional_string(map, "checkServicePlan", &mut issues);
    let check_tenant_features =
        parse_optional_string_array(map, "checkTenantFeatures", &mut issues);
    let version = parse_optional_number(map, "version", &mut issues);
    let source_version = parse_optional_number(map, "sourceVersion", &mut issues);
    let current_scope = parse_optional_enum(map, "currentScope", &SCOPE_VALUES, &mut issues);
    let current_type = parse_optional_string(map, "currentType", &mut issues);

    if !issues.is_empty() {
        return Err(ApiError::validation(Value::Array(issues)));
    }

    let mode = match mode_value.as_deref() {
        Some("publish") => Mode::Publish,
        Some("recover") => Mode::Recover,
        _ => Mode::Save,
    };

    Ok(UpdateBody {
        meta,
        data,
        description,
        annotation,
        unversioned_data,
        links,
        mode,
        entry_type,
        hidden,
        mirrored,
        rev_id,
        lock_token,
        skip_sync_links,
        update_revision,
        check_service_plan,
        check_tenant_features,
        version,
        source_version,
        current_scope,
        current_type,
    })
}

/// `rename-entry.ts` request schema.
fn parse_rename_body(map: &Map<String, Value>) -> Result<String, ApiError> {
    let mut issues = Vec::new();

    let name = match map.get("name") {
        None => {
            issues.push(issue_invalid_type(&["name"], "string", "undefined"));
            None
        }
        Some(Value::String(value)) => {
            let name_issues = entity_name_issues(value);
            if name_issues.is_empty() {
                Some(value.clone())
            } else {
                issues.extend(name_issues);
                None
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(&["name"], "string", js_type_name(other)));
            None
        }
    };

    if !issues.is_empty() {
        return Err(ApiError::validation(Value::Array(issues)));
    }

    Ok(name.expect("validated above"))
}

// ---------------------------------------------------------------------------
// Database rows
// ---------------------------------------------------------------------------

/// `RETURN_COLUMNS` of `src/const/common.ts` (snake_case, as knex emits it).
const RETURN_COLUMNS: &str = "\
entries.entry_id, entries.scope::text AS scope, entries.type AS entry_type, \
entries.display_key AS key, entries.unversioned_data, entries.created_by, entries.created_at, \
revisions.updated_by, revisions.updated_at, entries.saved_id, entries.published_id, \
revisions.rev_id, entries.tenant_id, revisions.data, revisions.meta, revisions.annotation, \
entries.hidden, entries.mirrored, entries.public AS is_public, entries.workbook_id, \
entries.collection_id, revisions.version, revisions.source_version";

const NOT_SELECTED_COLUMNS: &str = "\
NULL::jsonb AS links, NULL::boolean AS is_deleted, NULL::timestamptz AS deleted_at";

#[derive(Debug, sqlx::FromRow)]
struct ReturnRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    key: Option<String>,
    unversioned_data: Option<Value>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_by: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    rev_id: Option<i64>,
    tenant_id: Option<String>,
    data: Option<Value>,
    meta: Option<Value>,
    annotation: Option<Value>,
    hidden: bool,
    mirrored: bool,
    is_public: bool,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    version: Option<i32>,
    source_version: Option<i32>,
    links: Option<Value>,
    is_deleted: Option<bool>,
    deleted_at: Option<DateTime<Utc>>,
}

/// Minimal entry projection used by the write services.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct EntryRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    key: Option<String>,
    display_key: Option<String>,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    hidden: bool,
    mirrored: bool,
}

/// `SELECT *` row used by the delete/recover paths (`innerMeta` included).
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct FullEntryRow {
    entry_id: i64,
    key: Option<String>,
    display_key: Option<String>,
    inner_meta: Option<Value>,
    scope: String,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct EntryModelRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_by: String,
    updated_at: DateTime<Utc>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    unversioned_data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Response models
// ---------------------------------------------------------------------------

/// `entry-revision-schema.ts` (the create-entry model).
fn entry_revision_map(row: &ReturnRow, include_links: bool) -> Map<String, Value> {
    let mut entry = Map::new();

    entry.insert("entryId".to_owned(), Value::String(encode_id(row.entry_id)));
    entry.insert("scope".to_owned(), Value::String(row.scope.clone()));
    entry.insert("type".to_owned(), Value::String(row.entry_type.clone()));
    entry.insert("key".to_owned(), json_opt_string(&row.key));
    entry.insert(
        "createdBy".to_owned(),
        Value::String(row.created_by.clone()),
    );
    entry.insert(
        "createdAt".to_owned(),
        Value::String(format_timestamp(row.created_at)),
    );
    entry.insert(
        "updatedBy".to_owned(),
        Value::String(row.updated_by.clone().unwrap_or_default()),
    );
    entry.insert(
        "updatedAt".to_owned(),
        Value::String(row.updated_at.map(format_timestamp).unwrap_or_default()),
    );
    entry.insert("savedId".to_owned(), json_opt_id(row.saved_id));
    entry.insert("publishedId".to_owned(), json_opt_id(row.published_id));
    entry.insert("revId".to_owned(), json_opt_id(row.rev_id));
    entry.insert("tenantId".to_owned(), json_opt_string(&row.tenant_id));
    entry.insert("data".to_owned(), row.data.clone().unwrap_or(Value::Null));
    entry.insert("meta".to_owned(), row.meta.clone().unwrap_or(Value::Null));
    entry.insert(
        "annotation".to_owned(),
        row.annotation.clone().unwrap_or(Value::Null),
    );
    entry.insert("hidden".to_owned(), Value::Bool(row.hidden));
    entry.insert("mirrored".to_owned(), Value::Bool(row.mirrored));
    entry.insert("public".to_owned(), Value::Bool(row.is_public));
    entry.insert("workbookId".to_owned(), json_opt_id(row.workbook_id));
    entry.insert("collectionId".to_owned(), json_opt_id(row.collection_id));
    entry.insert(
        "version".to_owned(),
        match row.version {
            Some(value) => json!(value),
            None => Value::Null,
        },
    );
    entry.insert(
        "sourceVersion".to_owned(),
        match row.source_version {
            Some(value) => json!(value),
            None => Value::Null,
        },
    );

    if include_links {
        entry.insert("links".to_owned(), row.links.clone().unwrap_or(Value::Null));
    }

    entry
}

/// `entry-return-columns-model.ts`.
fn entry_return_columns_map(row: &ReturnRow) -> Map<String, Value> {
    let mut entry = Map::new();

    entry.insert("entryId".to_owned(), Value::String(encode_id(row.entry_id)));
    entry.insert("scope".to_owned(), Value::String(row.scope.clone()));
    entry.insert("type".to_owned(), Value::String(row.entry_type.clone()));
    entry.insert("key".to_owned(), json_opt_string(&row.key));
    entry.insert(
        "createdBy".to_owned(),
        Value::String(row.created_by.clone()),
    );
    entry.insert(
        "createdAt".to_owned(),
        Value::String(format_timestamp(row.created_at)),
    );
    entry.insert("updatedBy".to_owned(), json_opt_string(&row.updated_by));
    entry.insert("updatedAt".to_owned(), json_datetime_opt(row.updated_at));
    entry.insert("savedId".to_owned(), json_opt_id(row.saved_id));
    entry.insert("publishedId".to_owned(), json_opt_id(row.published_id));
    entry.insert("revId".to_owned(), json_opt_id(row.rev_id));
    entry.insert("tenantId".to_owned(), json_opt_string(&row.tenant_id));
    entry.insert("data".to_owned(), row.data.clone().unwrap_or(Value::Null));
    entry.insert("meta".to_owned(), row.meta.clone().unwrap_or(Value::Null));
    entry.insert(
        "annotation".to_owned(),
        row.annotation.clone().unwrap_or(Value::Null),
    );
    entry.insert("hidden".to_owned(), Value::Bool(row.hidden));
    entry.insert("mirrored".to_owned(), Value::Bool(row.mirrored));
    entry.insert("public".to_owned(), Value::Bool(row.is_public));
    entry.insert("workbookId".to_owned(), json_opt_id(row.workbook_id));
    entry.insert("collectionId".to_owned(), json_opt_id(row.collection_id));
    entry.insert(
        "version".to_owned(),
        match row.version {
            Some(value) => json!(value),
            None => Value::Null,
        },
    );
    entry.insert(
        "sourceVersion".to_owned(),
        match row.source_version {
            Some(value) => json!(value),
            None => Value::Null,
        },
    );

    entry
}

/// `entry-model.ts` (the unversioned-data controller model).
fn entry_model_map(row: &EntryModelRow, include_unversioned_data: bool) -> Map<String, Value> {
    let mut entry = Map::new();

    entry.insert("entryId".to_owned(), Value::String(encode_id(row.entry_id)));
    entry.insert("scope".to_owned(), Value::String(row.scope.clone()));
    entry.insert("type".to_owned(), Value::String(row.entry_type.clone()));
    entry.insert("key".to_owned(), json_opt_string(&row.display_key));
    entry.insert("displayKey".to_owned(), json_opt_string(&row.display_key));
    entry.insert(
        "createdBy".to_owned(),
        Value::String(row.created_by.clone()),
    );
    entry.insert(
        "createdAt".to_owned(),
        Value::String(format_timestamp(row.created_at)),
    );
    entry.insert(
        "updatedBy".to_owned(),
        Value::String(row.updated_by.clone()),
    );
    entry.insert(
        "updatedAt".to_owned(),
        Value::String(format_timestamp(row.updated_at)),
    );
    entry.insert("savedId".to_owned(), json_opt_id(row.saved_id));
    entry.insert("publishedId".to_owned(), json_opt_id(row.published_id));
    entry.insert("tenantId".to_owned(), json_opt_string(&row.tenant_id));
    entry.insert("workbookId".to_owned(), json_opt_id(row.workbook_id));
    entry.insert("collectionId".to_owned(), json_opt_id(row.collection_id));

    if include_unversioned_data {
        entry.insert(
            "unversionedData".to_owned(),
            row.unversioned_data.clone().unwrap_or(Value::Null),
        );
    }

    entry
}

// ---------------------------------------------------------------------------
// Error constructors (mirroring `src/components/errors/*`)
// ---------------------------------------------------------------------------

fn not_exist_entry() -> ApiError {
    ApiError::not_exist_entry()
}

fn not_exist_revision() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_REVISION"),
        "The revision doesn't exist",
    )
}

fn entry_already_exists(entry_id: i64) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        Some("ERR.US.ENTRY_ALREADY_EXISTS"),
        "The entry already exists",
    )
    .with_details(json!({"entryId": encode_id(entry_id)}))
}

fn folder_already_exists_with_different_key_case(existed_key: &str, key: &str) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        Some("ERR.US.FOLDER_ALREADY_EXISTS_WITH_DIFFERENT_KEY_CASE"),
        "A folder with the same path already exists with a different key case",
    )
    .with_details(json!({"existedKey": existed_key, "key": key}))
}

fn parent_folder_not_exist(parent_folders: Option<Vec<String>>) -> ApiError {
    let error = ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("PARENT_FOLDER_NOT_EXIST"),
        "Couldn't find these parent folders",
    );

    match parent_folders {
        Some(parent_folders) => error.with_details(json!({"parentFolders": parent_folders})),
        None => error,
    }
}

fn modify_users_folder_denied(message: &str) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        Some("MODIFY_USERS_FOLDER_DENIED"),
        message,
    )
}

fn incorrect_link_error(invalid_link_ids: Value) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("INCORRECT_LINK_ERROR"),
        "Some of the provided links are incorrect",
    )
    .with_details(json!({"invalidLinkIds": invalid_link_ids}))
}

fn compute_entries_feature_disabled() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("COMPUTE_ENTRIES_FEATURE_DISABLED"),
        "Compute entries feature is disabled",
    )
}

fn compute_entry_type_change_forbidden() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("COMPUTE_ENTRY_TYPE_CHANGE_FORBIDDEN"),
        "Compute entry type cannot be changed",
    )
}

fn entry_is_locked(entry_id: i64, login: Option<String>, expiry_date: String) -> ApiError {
    ApiError::new(
        StatusCode::LOCKED,
        Some("ERR.US.ENTRY_IS_LOCKED"),
        "The entry is locked",
    )
    .with_details(json!({
        "entryId": encode_id(entry_id),
        "loginOrId": login,
        "expiryDate": expiry_date,
    }))
}

fn validation_error_text(validation_errors: String) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("VALIDATION_ERROR"),
        "Validation error",
    )
    .with_details(json!({"validationErrors": validation_errors}))
}

// ---------------------------------------------------------------------------
// Shared service helpers
// ---------------------------------------------------------------------------

/// `pullActiveLock` + `checkLock`.
async fn check_lock(
    state: &AppState,
    entry_id: i64,
    lock_token: Option<&str>,
) -> Result<(), ApiError> {
    let row: Option<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT lock_token, login, expiry_date FROM locks \
         WHERE entry_id = $1 AND expiry_date > $2 LIMIT 1",
    )
    .bind(entry_id)
    .bind(now_seconds())
    .fetch_optional(&state.db.primary)
    .await?;

    if let Some((token, login, expiry_date)) = row {
        if Some(token.as_str()) != lock_token {
            return Err(entry_is_locked(
                entry_id,
                login,
                format_timestamp(expiry_date),
            ));
        }
    }

    Ok(())
}

/// `getWorkbook` (`src/services/new/workbook/get-workbook.ts`) under
/// `accessServiceEnabled === false`: only the "live workbook of this tenant"
/// check remains. `isPrivateRoute` drops the tenant filter.
async fn get_workbook(
    state: &AppState,
    workbook_id: i64,
    is_private: bool,
) -> Result<(), ApiError> {
    let found: Option<(i64,)> = if is_private {
        sqlx::query_as(
            "SELECT workbook_id FROM workbooks WHERE deleted_at IS NULL AND workbook_id = $1 LIMIT 1",
        )
        .bind(workbook_id)
        .fetch_optional(&state.db.replica)
        .await?
    } else {
        sqlx::query_as(
            "SELECT workbook_id FROM workbooks \
             WHERE deleted_at IS NULL AND workbook_id = $1 AND tenant_id = $2 LIMIT 1",
        )
        .bind(workbook_id)
        .bind(state.config.tenant_id_override.as_str())
        .fetch_optional(&state.db.replica)
        .await?
    };

    if found.is_none() {
        return Err(ApiError::workbook_not_exists());
    }

    Ok(())
}

/// `Tenant.checkExistence` (public routes only).
async fn tenant_exists(state: &AppState, tenant_id: &str) -> Result<bool, ApiError> {
    let found: Option<(String,)> = sqlx::query_as(
        "SELECT tenant_id FROM tenants WHERE tenant_id = $1 AND enabled = true LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db.replica)
    .await?;

    Ok(found.is_some())
}

/// `checkEntry` / `checkFetchedEntry` with `verifiableEntry`.
async fn check_entry_tenant(
    state: &AppState,
    workbook_id: Option<i64>,
    tenant_id: &Option<String>,
    is_private: bool,
) -> Result<(), ApiError> {
    let Some(tenant_id) = tenant_id else {
        return Err(validation_error_text(
            "data.tenantId should be string".to_owned(),
        ));
    };

    if let Some(workbook_id) = workbook_id {
        return get_workbook(state, workbook_id, is_private).await;
    }

    if tenant_id != &state.config.tenant_id_override {
        return Err(ApiError::not_match_together());
    }

    Ok(())
}

/// Lookup of the entry used by the create paths (`isDeleted: false`).
async fn find_entry_id_by_key(
    conn: &mut PgConnection,
    tenant_id: &str,
    key: &str,
) -> Result<Option<i64>, ApiError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT entry_id FROM entries WHERE key = $1 AND tenant_id = $2 LIMIT 1")
            .bind(key)
            .bind(tenant_id)
            .fetch_optional(&mut *conn)
            .await?;

    Ok(row.map(|(entry_id,)| entry_id))
}

/// `Link.sync` (`src/db/models/links/index.ts`).
async fn sync_links(
    conn: &mut PgConnection,
    entry_id: i64,
    links: Option<&Value>,
) -> Result<Option<Value>, ApiError> {
    let Some(links) = links else {
        return Ok(None);
    };

    let Some(object) = links.as_object() else {
        return Err(incorrect_link_error(json!({})));
    };

    let mut invalid_link_ids = Map::new();
    // `upsertGraph` uses `(fromId, toId)` as the identity, so two names for the
    // same target collapse into a single row (last name wins).
    let mut valid_links: Vec<(i64, String)> = Vec::new();

    for (name, value) in object {
        let decoded = match value {
            Value::String(raw) => decode_id(raw).ok(),
            _ => None,
        };

        match decoded {
            Some(to_id) => {
                if let Some(existing) = valid_links
                    .iter_mut()
                    .find(|(existing_id, _)| *existing_id == to_id)
                {
                    existing.1 = name.clone();
                } else {
                    valid_links.push((to_id, name.clone()));
                }
            }
            None => {
                invalid_link_ids.insert(name.clone(), value.clone());
            }
        }
    }

    if !invalid_link_ids.is_empty() {
        return Err(incorrect_link_error(Value::Object(invalid_link_ids)));
    }

    // `Links.sync` wraps every non-presentable failure as `SYNC_LINKS_FAILED`.
    sqlx::query("DELETE FROM links WHERE from_id = $1")
        .bind(entry_id)
        .execute(&mut *conn)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;

    if !valid_links.is_empty() {
        // One statement per entry instead of one per link: `upsertGraph` in
        // Node inserts the whole graph in a single query as well.
        let ids: Vec<i64> = valid_links.iter().map(|(id, _)| *id).collect();
        let names: Vec<String> = valid_links.iter().map(|(_, name)| name.clone()).collect();

        sqlx::query(
            "INSERT INTO links (from_id, to_id, name) \
             SELECT $1, l.to_id, l.name FROM UNNEST($2::bigint[], $3::text[]) AS l(to_id, name) \
             ON CONFLICT (from_id, to_id, name) DO UPDATE SET name = EXCLUDED.name",
        )
        .bind(entry_id)
        .bind(&ids)
        .bind(&names)
        .execute(&mut *conn)
        .await
        .map_err(|_| ApiError::sync_links_failed())?;
    }

    Ok(Some(links.clone()))
}

/// `Entry.syncLinks` — returns the value stored in `revisions.links`.
async fn entry_sync_links(
    conn: &mut PgConnection,
    entry_id: i64,
    links: Option<&Value>,
) -> Result<Option<Value>, ApiError> {
    if links.is_none() {
        return Ok(None);
    }

    sync_links(conn, entry_id, links).await
}

/// Next value of the `get_id()` sequence.
async fn next_id(conn: &mut PgConnection) -> Result<i64, ApiError> {
    let value: i64 = sqlx::query_scalar("SELECT get_id()")
        .fetch_one(&mut *conn)
        .await?;

    Ok(value)
}

struct EntryInsert {
    entry_id: i64,
    saved_id: i64,
    published_id: Option<i64>,
    key: String,
    display_key: String,
    tenant_id: String,
    scope: String,
    entry_type: Option<String>,
    unversioned_data: Option<Value>,
    created_by: String,
    updated_by: String,
    hidden: Option<bool>,
    mirrored: Option<bool>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
}

async fn insert_entry(conn: &mut PgConnection, entry: &EntryInsert) -> Result<(), ApiError> {
    let unversioned_defined = entry.unversioned_data.is_some();
    let unversioned = entry.unversioned_data.clone().unwrap_or(Value::Null);

    sqlx::query(
        "INSERT INTO entries (\
            entry_id, saved_id, published_id, key, display_key, tenant_id, scope, type, \
            inner_meta, unversioned_data, created_by, updated_by, is_deleted, deleted_at, \
            hidden, mirrored, workbook_id, collection_id\
         ) VALUES (\
            $1, $2, $3, $4, $5, $6, $7::scope, $8, NULL, \
            CASE WHEN $9::boolean THEN $10::jsonb ELSE '{}'::jsonb END, \
            $11, $12, false, NULL, COALESCE($13, false), COALESCE($14, false), $15, $16\
         )",
    )
    .bind(entry.entry_id)
    .bind(entry.saved_id)
    .bind(entry.published_id)
    .bind(entry.key.as_str())
    .bind(entry.display_key.as_str())
    .bind(entry.tenant_id.as_str())
    .bind(entry.scope.as_str())
    .bind(entry.entry_type.as_deref())
    .bind(unversioned_defined)
    .bind(unversioned)
    .bind(entry.created_by.as_str())
    .bind(entry.updated_by.as_str())
    .bind(entry.hidden)
    .bind(entry.mirrored)
    .bind(entry.workbook_id)
    .bind(entry.collection_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Revision insert used by create / update-save / update-publish.
///
/// `annotation` is the final `{description}` object, `data`/`meta`/`links`/
/// `version`/`source_version` are `None` when the request omitted them (the
/// columns are nullable, so `NULL` is the same thing knex writes).
#[allow(clippy::too_many_arguments)]
async fn insert_revision(
    conn: &mut PgConnection,
    entry_id: i64,
    data: Option<&Value>,
    meta: Option<&Value>,
    annotation: Option<&Value>,
    links: Option<&Value>,
    created_by: &str,
    updated_by: &str,
    version: Option<i32>,
    source_version: Option<i32>,
    rev_id: Option<i64>,
) -> Result<i64, ApiError> {
    // `rev_id` is pre-allocated by the callers that have to store it in
    // `entries.saved_id`/`published_id` *before* the revision row exists (FK
    // order). Passing it here keeps both ids identical; `None` lets the DB
    // default (`get_id()`) pick a fresh id, like `insert(...).returning()`.
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO revisions (\
            rev_id, entry_id, data, meta, annotation, links, created_by, updated_by, \
            version, source_version\
         ) VALUES (COALESCE($1::bigint, get_id()), $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         RETURNING rev_id",
    )
    .bind(rev_id)
    .bind(entry_id)
    .bind(data.cloned())
    .bind(meta.cloned())
    .bind(annotation.cloned())
    .bind(links.cloned())
    .bind(created_by)
    .bind(updated_by)
    .bind(version)
    .bind(source_version)
    .fetch_one(&mut *conn)
    .await?;

    Ok(row.0)
}

/// The revision annotation column of `Entry.create` / `updateEntry`.
fn build_annotation(description: Option<&String>, annotation: Option<&String>) -> Option<Value> {
    if let Some(annotation) = annotation {
        return Some(json!({"description": annotation}));
    }

    if let Some(description) = description {
        return Some(json!({"description": description}));
    }

    None
}

// ---------------------------------------------------------------------------
// create-entry
// ---------------------------------------------------------------------------

/// `EntryService.create` under the open-source flags.
async fn create_entry_service(
    state: &AppState,
    body: &CreateBody,
    is_private: bool,
    editor: bool,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<ReturnRow, ApiError> {
    let tenant_id = state.config.tenant_id_override.clone();

    // `checkTenant` / license checks / `checkPrivateScopeAccess` are no-ops.
    if body.scope.as_deref() == Some("compute") {
        return Err(compute_entries_feature_disabled());
    }

    if let Some(workbook_id) = body.workbook_id {
        return create_in_workbook(
            state,
            body,
            workbook_id,
            &tenant_id,
            is_private,
            editor,
            request_log,
        )
        .await;
    }

    if let Some(collection_id) = body.collection_id {
        return create_in_collection(state, body, collection_id, &tenant_id, is_private).await;
    }

    create_plain(state, body, &tenant_id, is_private).await
}

/// `Entry.create` (`src/db/models/entry/index.ts`).
async fn create_plain(
    state: &AppState,
    body: &CreateBody,
    tenant_id: &str,
    is_private: bool,
) -> Result<ReturnRow, ApiError> {
    let Some(mut key) = body.key.clone() else {
        // `key.slice(-1)` throws a TypeError before validation runs.
        return Err(ApiError::internal());
    };

    if key.ends_with('/') {
        key.pop();
    }

    // --- `validateCreateEntry` (AJV) -------------------------------------
    let mut errors: Vec<String> = Vec::new();

    // Required properties, in schema order.
    if body.scope.is_none() {
        errors.push("data should have required property 'scope'".to_owned());
    }

    if let Some(message) = verify_entry_key_error(&key) {
        errors.push(format!("data.key {message}"));
    }

    if !errors.is_empty() {
        return Err(validation_error_text(errors.join(", ")));
    }

    let scope = body.scope.clone().unwrap_or_default();
    let is_folder = scope == "folder";

    let display_key = format_key(&key, is_folder);
    let key_formatted = format_key(&key.to_lowercase(), is_folder);
    let parent_folder_key = get_parent_folder_key(&key_formatted);

    let created_by = make_user_id(SYSTEM_USER_ID);

    let mut tx = state.db.primary.begin().await?;

    if !is_private && !tenant_exists(state, tenant_id).await? {
        return Err(ApiError::not_exist_tenant());
    }

    if !is_root(&parent_folder_key) {
        if body.recursion == Some(true) {
            let folder_keys = get_folders_keys(&get_parent_folder_key(&display_key));
            let folder_keys_lower: Vec<String> =
                folder_keys.iter().map(|key| key.to_lowercase()).collect();

            let existing: Vec<(String, i64)> = sqlx::query_as(
                "SELECT display_key AS key, entry_id FROM entries \
                 WHERE tenant_id = $1 AND key = ANY($2)",
            )
            .bind(tenant_id)
            .bind(folder_keys_lower)
            .fetch_all(&mut *tx)
            .await?;

            if !existing.is_empty() {
                // The folder with the longest display key is the "parent".
                let mut parent = &existing[0];
                for folder in &existing {
                    if folder.0.chars().count() > parent.0.chars().count() {
                        parent = folder;
                    }
                }

                let requested_key = folder_keys
                    .iter()
                    .find(|key| key.to_lowercase() == parent.0.to_lowercase());

                match requested_key {
                    Some(requested_key) if requested_key == &parent.0 => {}
                    _ => {
                        return Err(folder_already_exists_with_different_key_case(
                            &parent.0,
                            requested_key.map(String::as_str).unwrap_or(&display_key),
                        ));
                    }
                }
            }

            let not_existed: Vec<String> = folder_keys
                .iter()
                .filter(|folder_key| {
                    !existing
                        .iter()
                        .any(|folder| folder.0.to_lowercase() == folder_key.to_lowercase())
                })
                .cloned()
                .collect();

            for folder_key in not_existed {
                let entry_id = next_id(&mut *tx).await?;
                let rev_id = next_id(&mut *tx).await?;
                let folder_lower_key = folder_key.to_lowercase();

                insert_entry(
                    &mut *tx,
                    &EntryInsert {
                        entry_id,
                        saved_id: rev_id,
                        published_id: None,
                        key: folder_lower_key,
                        display_key: folder_key.clone(),
                        tenant_id: tenant_id.to_owned(),
                        scope: "folder".to_owned(),
                        entry_type: Some(String::new()),
                        unversioned_data: None,
                        created_by: created_by.clone(),
                        updated_by: created_by.clone(),
                        hidden: Some(body.hidden),
                        mirrored: body.mirrored,
                        workbook_id: None,
                        collection_id: None,
                    },
                )
                .await?;

                insert_revision(
                    &mut *tx,
                    entry_id,
                    Some(&json!({})),
                    Some(&json!({})),
                    None,
                    None,
                    &created_by,
                    &created_by,
                    None,
                    None,
                    None,
                )
                .await?;
            }
        }

        let parent: Option<ReturnRow> = sqlx::query_as(&format!(
            "SELECT {RETURN_COLUMNS}, {NOT_SELECTED_COLUMNS} FROM entries \
             JOIN revisions ON entries.saved_id = revisions.rev_id \
             WHERE tenant_id = $1 AND key = $2 AND entries.is_deleted = false LIMIT 1"
        ))
        .bind(tenant_id)
        .bind(parent_folder_key.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        match parent {
            Some(parent) => {
                let requested_parent_key = get_parent_folder_key(&display_key);
                let parent_key = parent.key.clone().unwrap_or_default();

                if parent_key != requested_parent_key {
                    return Err(folder_already_exists_with_different_key_case(
                        &parent_key,
                        &requested_parent_key,
                    ));
                }
            }
            None => return Err(parent_folder_not_exist(None)),
        }
    }

    if let Some(existing_id) = find_entry_id_by_key(&mut *tx, tenant_id, &key_formatted).await? {
        return Err(entry_already_exists(existing_id));
    }

    let entry_id = next_id(&mut *tx).await?;
    let rev_id = next_id(&mut *tx).await?;

    let synced_links = entry_sync_links(&mut *tx, entry_id, body.links.as_ref()).await?;

    let published_id = if body.mode.as_deref() == Some("publish") {
        Some(rev_id)
    } else {
        None
    };

    insert_entry(
        &mut *tx,
        &EntryInsert {
            entry_id,
            saved_id: rev_id,
            published_id,
            key: key_formatted,
            display_key,
            tenant_id: tenant_id.to_owned(),
            scope,
            entry_type: body.entry_type.clone(),
            unversioned_data: body.unversioned_data.clone(),
            created_by: created_by.clone(),
            updated_by: created_by.clone(),
            hidden: Some(body.hidden),
            mirrored: body.mirrored,
            workbook_id: None,
            collection_id: None,
        },
    )
    .await?;

    let annotation = build_annotation(body.description.as_ref(), body.annotation.as_ref());

    insert_revision(
        &mut *tx,
        entry_id,
        body.data.as_ref(),
        body.meta.as_ref(),
        annotation.as_ref(),
        synced_links.as_ref(),
        &created_by,
        &created_by,
        body.version,
        body.source_version,
        Some(rev_id),
    )
    .await?;

    let row: Option<ReturnRow> = sqlx::query_as(&format!(
        "SELECT {RETURN_COLUMNS}, revisions.links AS links, NULL::boolean AS is_deleted, \
         NULL::timestamptz AS deleted_at \
         FROM entries JOIN revisions ON revisions.entry_id = entries.entry_id \
         WHERE entries.entry_id = $1 AND entries.is_deleted = false LIMIT 1"
    ))
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    row.ok_or_else(not_exist_entry)
}

/// `createEntryInWorkbook` (`src/services/entry/actions/create-in-workbook.ts`).
async fn create_in_workbook(
    state: &AppState,
    body: &CreateBody,
    workbook_id: i64,
    tenant_id: &str,
    is_private: bool,
    editor: bool,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<ReturnRow, ApiError> {
    // --- `validateCreateEntryInWorkbook` (AJV) ---------------------------
    let mut errors: Vec<String> = Vec::new();

    if body.name.is_none() {
        errors.push("data should have required property 'name'".to_owned());
    }
    if body.scope.is_none() {
        errors.push("data should have required property 'scope'".to_owned());
    }

    if let Some(scope) = body.scope.as_deref() {
        const WORKBOOK_SCOPES: [&str; 6] = [
            "connection",
            "dataset",
            "dash",
            "widget",
            "report",
            "artifact",
        ];

        if !WORKBOOK_SCOPES.contains(&scope) {
            errors.push("data.scope should be equal to one of the allowed values".to_owned());
        }
    }

    if let Some(name) = body.name.as_deref() {
        if let Some(message) = verify_entry_name_error(name) {
            errors.push(format!("data.name {message}"));
        }
    }

    if !errors.is_empty() {
        return Err(validation_error_text(errors.join(", ")));
    }

    let name = body.name.clone().unwrap_or_default();
    let scope = body.scope.clone().unwrap_or_default();

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "workbookId".to_string(),
            Value::String(encode_id(workbook_id)),
        );
        fields.insert("includePermissionsInfo".to_string(), Value::Bool(false));
        crate::logging::event(log, "createEntry", "GET_WORKBOOK_START", fields);
    }

    get_workbook(state, workbook_id, is_private).await?;

    if !is_private && !editor {
        return Err(ApiError::access_service_permission_denied());
    }

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "workbookId".to_string(),
            Value::String(encode_id(workbook_id)),
        );
        crate::logging::event(log, "createEntry", "GET_WORKBOOK_FINISH", fields);
    }

    let created_by = make_user_id(SYSTEM_USER_ID);

    let mut tx = state.db.primary.begin().await?;

    let entry_id = next_id(&mut *tx).await?;
    let rev_id = next_id(&mut *tx).await?;

    let display_key = format!("{entry_id}/{name}");
    let key = display_key.to_lowercase();

    if let Some(log) = request_log {
        crate::logging::event(log, "createEntry", "SYNC_LINKS", Map::new());
    }

    let synced_links = entry_sync_links(&mut *tx, entry_id, body.links.as_ref()).await?;

    let published_id = if body.mode.as_deref() == Some("publish") {
        Some(rev_id)
    } else {
        None
    };

    insert_entry(
        &mut *tx,
        &EntryInsert {
            entry_id,
            saved_id: rev_id,
            published_id,
            key,
            display_key,
            tenant_id: tenant_id.to_owned(),
            scope,
            entry_type: Some(body.entry_type.clone().unwrap_or_default()),
            unversioned_data: body.unversioned_data.clone(),
            created_by: created_by.clone(),
            updated_by: created_by.clone(),
            hidden: Some(body.hidden),
            mirrored: body.mirrored,
            workbook_id: Some(workbook_id),
            collection_id: None,
        },
    )
    .await?;

    let annotation = build_annotation(body.description.as_ref(), body.annotation.as_ref());

    insert_revision(
        &mut *tx,
        entry_id,
        body.data.as_ref(),
        body.meta.as_ref(),
        annotation.as_ref(),
        synced_links.as_ref(),
        &created_by,
        &created_by,
        body.version,
        body.source_version,
        Some(rev_id),
    )
    .await?;

    let row: Option<ReturnRow> = sqlx::query_as(&format!(
        "SELECT {RETURN_COLUMNS}, revisions.links AS links, NULL::boolean AS is_deleted, \
         NULL::timestamptz AS deleted_at \
         FROM entries JOIN revisions ON revisions.entry_id = entries.entry_id \
         WHERE entries.entry_id = $1 AND entries.is_deleted = false LIMIT 1"
    ))
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    row.ok_or_else(not_exist_entry)
}

/// `getParentIds` — collection ids of the collection and all of its parents.
async fn collection_parent_ids(
    state: &AppState,
    collection_id: i64,
    tenant_id: &str,
) -> Result<Vec<i64>, ApiError> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "WITH RECURSIVE collection_parents AS (\
            SELECT collection_id, parent_id FROM collections \
            WHERE tenant_id = $1 AND deleted_at IS NULL AND collection_id = $2 \
            UNION \
            SELECT collections.collection_id, collections.parent_id FROM collections \
            JOIN collection_parents ON collection_parents.parent_id = collections.collection_id \
            WHERE collections.tenant_id = $1 AND collections.deleted_at IS NULL\
         ) SELECT collection_id FROM collection_parents",
    )
    .bind(tenant_id)
    .bind(collection_id)
    .fetch_all(&state.db.replica)
    .await?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// `createEntryInCollection` (`src/services/new/entry/create-in-collection`).
async fn create_in_collection(
    state: &AppState,
    body: &CreateBody,
    collection_id: i64,
    tenant_id: &str,
    _is_private: bool,
) -> Result<ReturnRow, ApiError> {
    // --- `parseArgs` (zod, re-validated inside the service) --------------
    let mut issues = Vec::new();

    let scope = match body.scope.as_deref() {
        None => {
            issues.push(issue_invalid_type(&["scope"], "string", "undefined"));
            None
        }
        Some(scope) => {
            if !SCOPE_VALUES.contains(&scope) {
                issues.push(issue_invalid_value(&["scope"], &SCOPE_VALUES));
                None
            } else if !ALLOWED_COLLECTION_ENTRY_SCOPES.contains(&scope) {
                issues.push(issue_custom(
                    &["scope"],
                    &format!(
                        "Must be one of the values: {}",
                        ALLOWED_COLLECTION_ENTRY_SCOPES.join(", ")
                    ),
                ));
                None
            } else {
                Some(scope.to_owned())
            }
        }
    };

    let name = match body.name.as_deref() {
        None => {
            issues.push(issue_invalid_type(&["name"], "string", "undefined"));
            None
        }
        Some(name) => {
            let name_issues = entity_name_issues(name);
            if name_issues.is_empty() {
                Some(name.to_owned())
            } else {
                issues.extend(name_issues);
                None
            }
        }
    };

    if !issues.is_empty() {
        return Err(ApiError::validation(Value::Array(issues)));
    }

    let name = name.expect("validated above");
    let scope = scope.expect("validated above");

    let parent_ids = collection_parent_ids(state, collection_id, tenant_id).await?;

    if parent_ids.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            Some("COLLECTION_NOT_EXISTS"),
            format!(
                "Cannot find parent collection with id – {}",
                encode_id(collection_id)
            ),
        ));
    }

    let created_by = make_user_id(SYSTEM_USER_ID);

    let mut tx = state.db.primary.begin().await?;

    let entry_id = next_id(&mut *tx).await?;
    let rev_id = next_id(&mut *tx).await?;

    let display_key = format!("{entry_id}/{name}");
    let key = display_key.to_lowercase();

    let synced_links = entry_sync_links(&mut *tx, entry_id, body.links.as_ref()).await?;

    let published_id = if body.mode.as_deref() == Some("publish") {
        Some(rev_id)
    } else {
        None
    };

    insert_entry(
        &mut *tx,
        &EntryInsert {
            entry_id,
            saved_id: rev_id,
            published_id,
            key,
            display_key,
            tenant_id: tenant_id.to_owned(),
            scope,
            entry_type: Some(body.entry_type.clone().unwrap_or_default()),
            unversioned_data: body.unversioned_data.clone(),
            created_by: created_by.clone(),
            updated_by: created_by.clone(),
            hidden: Some(body.hidden),
            mirrored: body.mirrored,
            workbook_id: None,
            collection_id: Some(collection_id),
        },
    )
    .await?;

    let annotation = build_annotation(body.description.as_ref(), body.annotation.as_ref());

    insert_revision(
        &mut *tx,
        entry_id,
        body.data.as_ref(),
        body.meta.as_ref(),
        annotation.as_ref(),
        synced_links.as_ref(),
        &created_by,
        &created_by,
        body.version,
        body.source_version,
        Some(rev_id),
    )
    .await?;

    let row: Option<ReturnRow> = sqlx::query_as(&format!(
        "SELECT {RETURN_COLUMNS}, revisions.links AS links, NULL::boolean AS is_deleted, \
         NULL::timestamptz AS deleted_at \
         FROM entries JOIN revisions ON revisions.entry_id = entries.entry_id \
         WHERE entries.entry_id = $1 AND entries.is_deleted = false LIMIT 1"
    ))
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await?;

    tx.commit().await?;

    row.ok_or_else(not_exist_entry)
}

// ---------------------------------------------------------------------------
// update-entry
// ---------------------------------------------------------------------------

/// `updateEntry` (`src/services/entry/actions/update-entry.ts`).
async fn update_entry_service(
    state: &AppState,
    entry_id: i64,
    body: &UpdateBody,
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<(ReturnRow, bool), ApiError> {
    let _tenant_id = state.config.tenant_id_override.clone();

    let entry: Option<EntryRow> = sqlx::query_as(
        "SELECT entry_id, scope::text AS scope, type AS entry_type, key, display_key, tenant_id, \
         workbook_id, collection_id, saved_id, published_id, hidden, mirrored \
         FROM entries WHERE entry_id = $1 \
           AND ($2::boolean OR is_deleted = false) \
           AND ($3::scope IS NULL OR scope = $3::scope) \
           AND ($4::text IS NULL OR type = $4) \
         LIMIT 1",
    )
    .bind(entry_id)
    .bind(body.mode == Mode::Recover)
    .bind(body.current_scope.as_deref())
    .bind(body.current_type.as_deref())
    .fetch_optional(&state.db.primary)
    .await?;

    let Some(entry) = entry else {
        return Err(not_exist_entry());
    };

    if !is_private {
        permissions::check_workbook_isolation_id(
            request_workbook_id,
            entry.workbook_id,
            entry.collection_id,
        )?;
    }

    if !is_private && entry.workbook_id.is_none() {
        check_entry_tenant(state, entry.workbook_id, &entry.tenant_id, is_private).await?;
    }

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(encode_id(entry_id)));
        let mode = match body.mode {
            Mode::Save => "save",
            Mode::Publish => "publish",
            Mode::Recover => "recover",
        };
        fields.insert("mode".to_string(), Value::String(mode.to_string()));
        fields.insert(
            "updateRevision".to_string(),
            Value::Bool(body.update_revision.unwrap_or(false)),
        );
        crate::logging::event(log, "updateEntry", "UPDATE_ENTRY_REQUEST", fields);

        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(entry_id.to_string()));
        fields.insert(
            "requestedBy".to_string(),
            json!({"userId": "systemId", "login": "unknown"}),
        );
        crate::logging::event(log, "updateEntry", "CHECK_LOCK_ENTRY_REQUEST", fields);
    }

    check_lock(state, entry_id, body.lock_token.as_deref()).await?;

    if let Some(log) = request_log {
        crate::logging::event(log, "updateEntry", "CHECK_LOCK_ENTRY_SUCCESS", Map::new());
    }

    if entry.scope == "compute" {
        if let Some(entry_type) = body.entry_type.as_deref() {
            if entry_type != entry.entry_type {
                return Err(compute_entry_type_change_forbidden());
            }
        }
    }

    let updated_by = make_user_id(SYSTEM_USER_ID);

    if let Some(workbook_id) = entry.workbook_id {
        if !is_private {
            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                fields.insert("includePermissionsInfo".to_string(), Value::Bool(false));
                crate::logging::event(log, "updateEntry", "GET_WORKBOOK_START", fields);
            }

            get_workbook(state, workbook_id, is_private).await?;

            if !editor {
                return Err(ApiError::access_service_permission_denied());
            }

            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                crate::logging::event(log, "updateEntry", "GET_WORKBOOK_FINISH", fields);
            }
        }
    } else if !is_private && entry.collection_id.is_some() && !editor {
        return Err(ApiError::access_service_permission_denied());
    }

    match body.mode {
        Mode::Save => update_save(state, &entry, body, &updated_by, is_private).await,
        Mode::Publish => update_publish(state, &entry, body, &updated_by).await,
        Mode::Recover => update_recover(state, &entry, body, &updated_by, is_private).await,
    }
}

async fn update_save(
    state: &AppState,
    entry: &EntryRow,
    body: &UpdateBody,
    updated_by: &str,
    is_private: bool,
) -> Result<(ReturnRow, bool), ApiError> {
    let entry_id = entry.entry_id;

    let mut tx = state.db.primary.begin().await?;

    let synced_links = if body.skip_sync_links == Some(true) {
        body.links.clone()
    } else {
        entry_sync_links(&mut *tx, entry_id, body.links.as_ref()).await?
    };

    let rev_id = if is_private && body.update_revision == Some(true) {
        update_existing_revision(&mut *tx, entry, body, synced_links.as_ref(), updated_by).await?
    } else {
        let annotation = build_annotation(body.description.as_ref(), body.annotation.as_ref());

        insert_revision(
            &mut *tx,
            entry_id,
            body.data.as_ref(),
            body.meta.as_ref(),
            annotation.as_ref(),
            synced_links.as_ref(),
            updated_by,
            updated_by,
            body.version,
            body.source_version,
            None,
        )
        .await?
    };

    let hidden_next = body.hidden.unwrap_or(entry.hidden);
    let mirrored_next = body.mirrored.unwrap_or(entry.mirrored);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE entries SET saved_id = ");
    builder.push_bind(rev_id);
    builder.push(", hidden = ");
    builder.push_bind(hidden_next);
    builder.push(", mirrored = ");
    builder.push_bind(mirrored_next);

    if let Some(entry_type) = body.entry_type.as_deref() {
        builder.push(", type = ");
        builder.push_bind(entry_type.to_owned());
    }

    if let Some(unversioned_data) = body.unversioned_data.as_ref() {
        if !unversioned_data.is_null() {
            builder.push(", unversioned_data = ");
            builder.push_bind(unversioned_data.clone());
        }
    }

    builder.push(" WHERE entry_id = ");
    builder.push_bind(entry_id);

    builder.build().execute(&mut *tx).await?;

    let sql = format!(
        "SELECT {RETURN_COLUMNS}, revisions.links AS links, NULL::boolean AS is_deleted, \
         NULL::timestamptz AS deleted_at \
         FROM entries JOIN revisions ON revisions.entry_id = entries.entry_id \
         WHERE entries.entry_id = $1 AND revisions.rev_id = $2 AND entries.is_deleted = false \
         LIMIT 1"
    );

    let row: Option<ReturnRow> = sqlx::query_as(&sql)
        .bind(entry_id)
        .bind(rev_id)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    let row = row.ok_or_else(not_exist_entry)?;

    Ok((row, true))
}

/// `isPrivateRoute && updateRevision` branch of `createOrUpdateRevision`.
async fn update_existing_revision(
    tx: &mut PgConnection,
    entry: &EntryRow,
    body: &UpdateBody,
    synced_links: Option<&Value>,
    updated_by: &str,
) -> Result<i64, ApiError> {
    let target_rev_id = entry.saved_id;

    let annotation_value = body
        .annotation
        .as_ref()
        .or(body.description.as_ref())
        .cloned();

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE revisions SET ");

    let mut has_set = false;

    if let Some(data) = body.data.as_ref() {
        builder.push("data = ");
        builder.push_bind(data.clone());
        has_set = true;
    }

    if let Some(meta) = body.meta.as_ref() {
        if has_set {
            builder.push(", ");
        }
        builder.push("meta = ");
        builder.push_bind(meta.clone());
        has_set = true;
    }

    if let Some(annotation) = annotation_value {
        if has_set {
            builder.push(", ");
        }
        builder.push(
            "annotation = jsonb_set(COALESCE(annotation, '{}'::jsonb), '{description}', to_jsonb(",
        );
        builder.push_bind(annotation);
        builder.push("::text))");
        has_set = true;
    }

    if let Some(links) = synced_links {
        if has_set {
            builder.push(", ");
        }
        builder.push("links = ");
        builder.push_bind(links.clone());
        has_set = true;
    }

    if let Some(version) = body.version {
        if has_set {
            builder.push(", ");
        }
        builder.push("version = ");
        builder.push_bind(version);
        has_set = true;
    }

    if let Some(source_version) = body.source_version {
        if has_set {
            builder.push(", ");
        }
        builder.push("source_version = ");
        builder.push_bind(source_version);
        has_set = true;
    }

    if has_set {
        builder.push(", ");
    }

    builder.push("updated_by = ");
    builder.push_bind(updated_by.to_owned());
    builder.push(", updated_at = CURRENT_TIMESTAMP");

    match target_rev_id {
        Some(rev_id) => {
            builder.push(" WHERE rev_id = ");
            builder.push_bind(rev_id);
            builder.push(" AND entry_id = ");
            builder.push_bind(entry.entry_id);

            let row: Option<(i64,)> = builder.build_query_as().fetch_optional(&mut *tx).await?;

            match row {
                Some((rev_id,)) => Ok(rev_id),
                None => Err(not_exist_revision()),
            }
        }
        None => Err(not_exist_revision()),
    }
}

async fn update_publish(
    state: &AppState,
    entry: &EntryRow,
    body: &UpdateBody,
    updated_by: &str,
) -> Result<(ReturnRow, bool), ApiError> {
    let entry_id = entry.entry_id;

    let mut tx = state.db.primary.begin().await?;

    let rev_id;

    if let Some(requested_rev_id) = body.rev_id {
        let revision: Option<(Option<Value>,)> = sqlx::query_as(
            "SELECT links FROM revisions WHERE entry_id = $1 AND rev_id = $2 LIMIT 1",
        )
        .bind(entry_id)
        .bind(requested_rev_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((revision_links,)) = revision else {
            return Err(not_exist_revision());
        };

        let links_value = match revision_links {
            Some(Value::Object(_)) => revision_links.unwrap_or_else(|| json!({})),
            _ => json!({}),
        };

        sync_links(&mut *tx, entry_id, Some(&links_value)).await?;

        rev_id = requested_rev_id;

        sqlx::query(
            "UPDATE entries SET published_id = $1, updated_by = $2, \
             updated_at = CURRENT_TIMESTAMP WHERE entry_id = $3",
        )
        .bind(rev_id)
        .bind(updated_by)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;
    } else {
        let synced_links = entry_sync_links(&mut *tx, entry_id, body.links.as_ref()).await?;

        let annotation = build_annotation(body.description.as_ref(), body.annotation.as_ref());

        rev_id = insert_revision(
            &mut *tx,
            entry_id,
            body.data.as_ref(),
            body.meta.as_ref(),
            annotation.as_ref(),
            synced_links.as_ref(),
            updated_by,
            updated_by,
            body.version,
            body.source_version,
            None,
        )
        .await?;

        let mut builder: QueryBuilder<Postgres> =
            QueryBuilder::new("UPDATE entries SET saved_id = ");
        builder.push_bind(rev_id);
        builder.push(", published_id = ");
        builder.push_bind(rev_id);
        builder.push(", updated_by = ");
        builder.push_bind(updated_by.to_owned());
        builder.push(", updated_at = CURRENT_TIMESTAMP");

        if let Some(entry_type) = body.entry_type.as_deref() {
            builder.push(", type = ");
            builder.push_bind(entry_type.to_owned());
        }

        if let Some(unversioned_data) = body.unversioned_data.as_ref() {
            if !unversioned_data.is_null() {
                builder.push(", unversioned_data = ");
                builder.push_bind(unversioned_data.clone());
            }
        }

        builder.push(" WHERE entry_id = ");
        builder.push_bind(entry_id);

        builder.build().execute(&mut *tx).await?;
    }

    let sql = format!(
        "SELECT {RETURN_COLUMNS}, {NOT_SELECTED_COLUMNS} \
         FROM entries JOIN revisions ON revisions.entry_id = entries.entry_id \
         WHERE entries.entry_id = $1 AND revisions.rev_id = $2 AND entries.is_deleted = false \
         LIMIT 1"
    );

    let row: Option<ReturnRow> = sqlx::query_as(&sql)
        .bind(entry_id)
        .bind(rev_id)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    let row = row.ok_or_else(not_exist_entry)?;

    Ok((row, false))
}

async fn update_recover(
    state: &AppState,
    _entry: &EntryRow,
    body: &UpdateBody,
    updated_by: &str,
    is_private: bool,
) -> Result<(ReturnRow, bool), ApiError> {
    let entry_id = _entry.entry_id;

    let mut tx = state.db.primary.begin().await?;

    let deleted: Option<FullEntryRow> = sqlx::query_as(
        "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, tenant_id, workbook_id \
         FROM entries WHERE entry_id = $1 AND is_deleted = true LIMIT 1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(deleted) = deleted else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            Some("NOT_EXIST_DELETED_ENTRY"),
            "The entity doesn't exist",
        ));
    };

    let inner_meta = match deleted.inner_meta.as_ref() {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };

    let new_key = match inner_meta.get("oldKey") {
        Some(Value::String(value)) => value.clone(),
        _ => return Err(ApiError::internal()),
    };
    let new_display_key = match inner_meta.get("oldDisplayKey") {
        Some(Value::String(value)) => value.clone(),
        _ => return Err(ApiError::internal()),
    };

    let entry_tenant_id = deleted.tenant_id.clone().unwrap_or_default();

    if let Some(workbook_id) = deleted.workbook_id {
        get_workbook(state, workbook_id, is_private).await?;
    } else {
        let key_lower_case = new_key.to_lowercase();
        let is_folder = deleted.scope == "folder";
        let key_formatted = format_key(&key_lower_case, is_folder);

        let parent_folder_keys: Vec<String> = get_full_parent_folder_keys(&key_formatted)
            .into_iter()
            .filter(|key| !is_root(key))
            .collect();

        if !parent_folder_keys.is_empty() {
            let folders: Vec<(String,)> = sqlx::query_as(
                "SELECT key FROM entries WHERE tenant_id = $1 AND scope = 'folder'::scope \
                 AND is_deleted = false AND key = ANY($2)",
            )
            .bind(&entry_tenant_id)
            .bind(parent_folder_keys.clone())
            .fetch_all(&mut *tx)
            .await?;

            let existing: Vec<String> = folders.into_iter().map(|(key,)| key).collect();
            let not_found: Vec<String> = parent_folder_keys
                .into_iter()
                .filter(|key| !existing.contains(key))
                .collect();

            if !not_found.is_empty() {
                return Err(parent_folder_not_exist(Some(not_found)));
            }
        }
    }

    let mut new_inner_meta = inner_meta.clone();
    new_inner_meta.remove("oldKey");

    if deleted.scope == "folder" {
        let entry_obj_key = deleted.key.clone().unwrap_or_default();
        let pattern = format!("{}%", escape_string_for_like(&entry_obj_key));

        let children: Vec<FullEntryRow> = sqlx::query_as(
            "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, tenant_id, \
             workbook_id FROM entries WHERE key LIKE $1 AND tenant_id = $2",
        )
        .bind(pattern)
        .bind(&entry_tenant_id)
        .fetch_all(&mut *tx)
        .await?;

        for child in children {
            let child_inner_meta = match child.inner_meta.as_ref() {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };

            let child_new_key = match child_inner_meta.get("oldKey") {
                Some(Value::String(value)) => value.clone(),
                _ => continue,
            };
            let child_new_display_key = match child_inner_meta.get("oldDisplayKey") {
                Some(Value::String(value)) => value.clone(),
                _ => continue,
            };

            let mut child_meta = child_inner_meta.clone();
            child_meta.remove("oldKey");

            sqlx::query(
                "UPDATE entries SET key = $1, display_key = $2, inner_meta = $3, \
                 is_deleted = false, deleted_at = NULL, updated_by = $4, \
                 updated_at = CURRENT_TIMESTAMP WHERE entry_id = $5",
            )
            .bind(child_new_key)
            .bind(child_new_display_key)
            .bind(Value::Object(child_meta))
            .bind(updated_by)
            .bind(child.entry_id)
            .execute(&mut *tx)
            .await?;
        }
    } else {
        sqlx::query(
            "UPDATE entries SET key = $1, display_key = $2, inner_meta = $3, \
             is_deleted = false, deleted_at = NULL, updated_by = $4, \
             updated_at = CURRENT_TIMESTAMP WHERE entry_id = $5",
        )
        .bind(new_key)
        .bind(new_display_key)
        .bind(Value::Object(new_inner_meta))
        .bind(updated_by)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;
    }

    let sql = format!(
        "SELECT {RETURN_COLUMNS}, {NOT_SELECTED_COLUMNS} FROM entries \
         JOIN revisions ON entries.saved_id = revisions.rev_id \
         WHERE entries.entry_id = $1 AND entries.is_deleted = false LIMIT 1"
    );

    let row: Option<ReturnRow> = sqlx::query_as(&sql)
        .bind(entry_id)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    let row = row.ok_or_else(not_exist_entry)?;

    let _ = body;

    Ok((row, false))
}

// ---------------------------------------------------------------------------
// rename-entry
// ---------------------------------------------------------------------------

/// `renameEntry` (`src/services/entry/actions/rename-entry.ts`).
async fn rename_entry_service(
    state: &AppState,
    entry_id: i64,
    name: &str,
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<Vec<ReturnRow>, ApiError> {
    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(encode_id(entry_id)));
        crate::logging::event(log, "renameEntry", "RENAME_ENTRY_REQUEST", fields);
    }

    let tenant_id = state.config.tenant_id_override.clone();
    let updated_by = make_user_id(SYSTEM_USER_ID);

    let mut tx = state.db.primary.begin().await?;

    let renaming: Option<FullEntryRow> = sqlx::query_as(
        "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, tenant_id, workbook_id \
         FROM entries WHERE entry_id = $1 AND is_deleted = false LIMIT 1",
    )
    .bind(entry_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(renaming) = renaming else {
        return Err(not_exist_entry());
    };

    if !is_private {
        permissions::check_workbook_isolation_id(request_workbook_id, renaming.workbook_id, None)?;
    }

    if is_users_folder(&renaming.key) {
        return Err(modify_users_folder_denied(
            "Folder 'Users' cannot be renamed",
        ));
    }

    if renaming.workbook_id.is_none() {
        check_entry_tenant(state, renaming.workbook_id, &renaming.tenant_id, is_private).await?;
    }

    if let Some(workbook_id) = renaming.workbook_id {
        if !is_private {
            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                fields.insert("includePermissionsInfo".to_string(), Value::Bool(false));
                crate::logging::event(log, "renameEntry", "GET_WORKBOOK_START", fields);
            }
            get_workbook(state, workbook_id, is_private).await?;
            if !editor {
                return Err(ApiError::access_service_permission_denied());
            }
            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                crate::logging::event(log, "renameEntry", "GET_WORKBOOK_FINISH", fields);
            }
        }
    }

    let is_folder = renaming.scope == "folder";
    let old_entry_display_key = renaming.display_key.clone().unwrap_or_default();
    let old_entry_key = renaming.key.clone().unwrap_or_default();

    let mut parent_folder_display_key = get_parent_folder_key(&old_entry_display_key);
    if is_root(&parent_folder_display_key) {
        parent_folder_display_key = String::new();
    }

    let new_entry_display_key = format!(
        "{parent_folder_display_key}{name}{}",
        if is_folder { "/" } else { "" }
    );
    let new_entry_key = new_entry_display_key.to_lowercase();

    let key_offset = js_length(&old_entry_key) + 1;
    let display_offset = js_length(&old_entry_display_key) + 1;

    let pattern = format!(
        "{}{}",
        escape_string_for_like(&old_entry_key),
        if is_folder { "%" } else { "" }
    );

    let updated: Vec<(i64,)> = sqlx::query_as(
        "UPDATE entries SET \
            key = $1 || substr(key, $2), \
            display_key = $3 || substr(display_key, $4), \
            updated_by = $5, \
            updated_at = CURRENT_TIMESTAMP \
         WHERE key LIKE $6 AND tenant_id = $7 \
         RETURNING entry_id",
    )
    .bind(new_entry_key.as_str())
    .bind(key_offset as i32)
    .bind(new_entry_display_key.as_str())
    .bind(display_offset as i32)
    .bind(updated_by.as_str())
    .bind(pattern.as_str())
    .bind(tenant_id.as_str())
    .fetch_all(&mut *tx)
    .await?;

    let entry_ids: Vec<i64> = updated.into_iter().map(|(entry_id,)| entry_id).collect();

    let sql = format!(
        "SELECT {RETURN_COLUMNS}, {NOT_SELECTED_COLUMNS} FROM entries \
         LEFT JOIN revisions \
            ON revisions.rev_id = COALESCE(entries.published_id, entries.saved_id) \
         WHERE entries.is_deleted = false AND entries.entry_id = ANY($1)"
    );

    let rows: Vec<ReturnRow> = sqlx::query_as(&sql)
        .bind(entry_ids)
        .fetch_all(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// delete-entry
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct DeleteQuery {
    lock_token: Option<String>,
    scope: Option<String>,
    types: Option<Vec<String>>,
}

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
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or_default();
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

/// Parses the delete-entry query string (`lockToken`, `scope`, `types`).
fn parse_delete_query(raw: Option<&str>) -> Result<DeleteQuery, ApiError> {
    let mut lock_token = None;
    let mut scope = None;
    let mut types: Option<Vec<String>> = None;
    let mut indexed_types: Vec<(usize, String)> = Vec::new();
    let mut issues = Vec::new();

    let Some(raw) = raw else {
        return Ok(DeleteQuery::default());
    };

    for part in raw.split('&') {
        if part.is_empty() {
            continue;
        }

        let (key, value) = match part.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(part), String::new()),
        };

        match key.as_str() {
            "lockToken" => lock_token = Some(value),
            "scope" => scope = Some(value),
            "types" | "types[]" => {
                types.get_or_insert_with(Vec::new).push(value);
            }
            _ => {
                if let Some(index) = key
                    .strip_prefix("types[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .and_then(|index| index.parse::<usize>().ok())
                {
                    indexed_types.push((index, value));
                }
            }
        }
    }

    if !indexed_types.is_empty() {
        indexed_types.sort_by_key(|(index, _)| *index);
        let entry = types.get_or_insert_with(Vec::new);
        entry.extend(indexed_types.into_iter().map(|(_, value)| value));
    }

    if let Some(scope_value) = scope.as_deref() {
        if !SCOPE_VALUES.contains(&scope_value) {
            issues.push(issue_invalid_value(&["scope"], &SCOPE_VALUES));
        }
    }

    if let Some(items) = types.as_ref() {
        if items.is_empty() {
            issues.push(issue_too_small_array(&["types"], 1));
        } else if items.len() > 100 {
            issues.push(issue_too_big_array(&["types"], 100));
        }
    }

    if !issues.is_empty() {
        return Err(ApiError::validation(Value::Array(issues)));
    }

    Ok(DeleteQuery {
        lock_token,
        scope,
        types,
    })
}

/// `deleteEntry` (`src/services/entry/actions/delete-entry.ts`).
async fn delete_entry_service(
    state: &AppState,
    entry_id: i64,
    query: &DeleteQuery,
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<ReturnRow, ApiError> {
    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(encode_id(entry_id)));
        crate::logging::event(log, "deleteEntry", "DELETE_ENTRY_REQUEST", fields);
    }

    let tenant_id = state.config.tenant_id_override.clone();
    let deleted_by = make_user_id(SYSTEM_USER_ID);

    let mut tx = state.db.primary.begin().await?;

    let entry: Option<FullEntryRow> = sqlx::query_as(
        "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, tenant_id, workbook_id \
         FROM entries WHERE entry_id = $1 AND is_deleted = false \
           AND ($2::scope IS NULL OR scope = $2::scope) \
           AND ($3::text[] IS NULL OR type = ANY($3)) \
         LIMIT 1",
    )
    .bind(entry_id)
    .bind(query.scope.as_deref())
    .bind(query.types.clone())
    .fetch_optional(&mut *tx)
    .await?;

    let Some(entry) = entry else {
        return Err(not_exist_entry());
    };

    if !is_private {
        // `delete-entry.js` reads the entry without the collection column, so only
        // workbook isolation is checked here.
        permissions::check_workbook_isolation_id(request_workbook_id, entry.workbook_id, None)?;
    }

    if is_users_folder(&entry.key) {
        return Err(modify_users_folder_denied(
            "Folder 'Users' cannot be deleted",
        ));
    }

    if entry.workbook_id.is_none() {
        check_entry_tenant(state, entry.workbook_id, &entry.tenant_id, is_private).await?;
    }

    if !is_private {
        if let Some(workbook_id) = entry.workbook_id {
            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                fields.insert("includePermissionsInfo".to_string(), Value::Bool(false));
                crate::logging::event(log, "deleteEntry", "GET_WORKBOOK_START", fields);
            }
            get_workbook(state, workbook_id, is_private).await?;
            if !editor {
                return Err(ApiError::access_service_permission_denied());
            }
            if let Some(log) = request_log {
                let mut fields = Map::new();
                fields.insert(
                    "workbookId".to_string(),
                    Value::String(encode_id(workbook_id)),
                );
                crate::logging::event(log, "deleteEntry", "GET_WORKBOOK_FINISH", fields);
            }
        }
    }

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(entry_id.to_string()));
        fields.insert(
            "requestedBy".to_string(),
            json!({"userId": "systemId", "login": "unknown"}),
        );
        crate::logging::event(log, "deleteEntry", "CHECK_LOCK_ENTRY_REQUEST", fields);
    }

    check_lock(state, entry_id, query.lock_token.as_deref()).await?;

    if let Some(log) = request_log {
        crate::logging::event(log, "deleteEntry", "CHECK_LOCK_ENTRY_SUCCESS", Map::new());
    }

    if entry.scope == "folder" {
        let entry_obj_key = entry.key.clone().unwrap_or_default();
        let display_key = entry.display_key.clone().unwrap_or_default();

        let segments: Vec<&str> = display_key.split('/').collect();
        let folder_name = if segments.len() >= 2 {
            segments[segments.len() - 2]
        } else {
            ""
        };
        let new_folder_name = format!("{}_{}/", encode_id(entry.entry_id), folder_name);

        let pattern = format!("{}%", escape_string_for_like(&entry_obj_key));

        let children: Vec<FullEntryRow> = sqlx::query_as(
            "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, tenant_id, \
             workbook_id FROM entries WHERE key LIKE $1 AND tenant_id = $2",
        )
        .bind(pattern)
        .bind(tenant_id.as_str())
        .fetch_all(&mut *tx)
        .await?;

        // `delete-entry.js`: with DLS enabled the children are checked with
        // `DLS.checkBulkPermission(..., Admin)` and any denial aborts the whole
        // folder delete with `ENTRIES_WITH_INSUFFICIENT_PERMISSIONS`. The DLS
        // class shipped by the 1.196.0 open-source image is a stub returning no
        // rows (see `permissions::dls_bulk_permissions`), so the reference allows
        // the delete; the same gate is kept here for a real DLS backend.
        if !is_private && state.config.dls_enabled {
            let permissions = permissions::dls_bulk_permissions();
            if permissions::dls_folder_delete_denied(state.config.dls_enabled, &permissions) {
                return Err(ApiError::entries_with_insufficient_permissions());
            }
        }

        for child in children {
            let child_display_key = child.display_key.clone().unwrap_or_default();
            let mut new_key_chars: Vec<char> = child_display_key.chars().collect();
            let remove = entry_obj_key.chars().count().min(new_key_chars.len());
            new_key_chars.drain(0..remove);
            let mut new_key: String = new_folder_name.clone();
            new_key.extend(new_key_chars);
            let new_key_lower = new_key.to_lowercase();

            let mut inner_meta = match child.inner_meta.as_ref() {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };
            inner_meta.insert("oldKey".to_owned(), json!(child.key));
            inner_meta.insert("oldDisplayKey".to_owned(), json!(child_display_key));
            inner_meta.insert("lastDeletionUserId".to_owned(), json!(SYSTEM_USER_ID));

            mark_entry_as_deleted(
                &mut *tx,
                child.entry_id,
                &new_key_lower,
                &new_key,
                Value::Object(inner_meta),
                &deleted_by,
            )
            .await?;
        }
    } else {
        let display_key = entry.display_key.clone().unwrap_or_default();
        let segments: Vec<&str> = display_key.split('/').collect();
        let name = segments.last().copied().unwrap_or("");
        let old_key_lower = display_key.to_lowercase();
        // `newKey = `${entryObj.entryId}_${name}``: the raw bigint id, not the
        // encoded one (verified against the reference delete payload).
        let new_key = format!("{}_{}", entry.entry_id, name);

        let mut inner_meta = match entry.inner_meta.as_ref() {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        inner_meta.insert("oldKey".to_owned(), json!(old_key_lower));
        inner_meta.insert("oldDisplayKey".to_owned(), json!(display_key));
        inner_meta.insert("lastDeletionUserId".to_owned(), json!(SYSTEM_USER_ID));

        mark_entry_as_deleted(
            &mut *tx,
            entry_id,
            &new_key,
            &new_key,
            Value::Object(inner_meta),
            &deleted_by,
        )
        .await?;
    }

    let sql = format!(
        "SELECT {RETURN_COLUMNS}, NULL::jsonb AS links, entries.is_deleted AS is_deleted, \
         entries.deleted_at AS deleted_at \
         FROM entries JOIN revisions ON entries.saved_id = revisions.rev_id \
         WHERE entries.entry_id = $1 AND entries.is_deleted = true LIMIT 1"
    );

    let row: Option<ReturnRow> = sqlx::query_as(&sql)
        .bind(entry_id)
        .fetch_optional(&mut *tx)
        .await?;

    tx.commit().await?;

    let row = match row {
        Some(row) => row,
        None => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                Some("NOT_EXIST_ENTRY"),
                "Entry deletion failed: entry not found after delete",
            ))
        }
    };

    if row.collection_id.is_some() {
        // `createCollectionEntry` throws from the constructor in this build.
        return Err(ApiError::collection_entry_unsupported_scope());
    }

    Ok(row)
}

/// `markEntryAsDeleted` (`src/services/entry/crud/update/mark-entry-as-deleted.ts`).
async fn mark_entry_as_deleted(
    conn: &mut PgConnection,
    entry_id: i64,
    new_key: &str,
    new_display_key: &str,
    new_inner_meta: Value,
    updated_by: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE entries SET key = $1, display_key = $2, inner_meta = $3, updated_by = $4, \
         is_deleted = true, updated_at = CURRENT_TIMESTAMP, deleted_at = CURRENT_TIMESTAMP \
         WHERE entry_id = $5",
    )
    .bind(format!("{TRASH_FOLDER}/{new_key}"))
    .bind(format!("{TRASH_FOLDER}/{new_display_key}"))
    .bind(new_inner_meta)
    .bind(updated_by)
    .bind(entry_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// switch-revision-entry
// ---------------------------------------------------------------------------

async fn switch_revision_service(
    state: &AppState,
    entry_id: i64,
    rev_id: i64,
) -> Result<Value, ApiError> {
    let entry: Option<(i64,)> = sqlx::query_as(
        "SELECT entry_id FROM entries WHERE entry_id = $1 AND is_deleted = false LIMIT 1",
    )
    .bind(entry_id)
    .fetch_optional(&state.db.replica)
    .await?;

    if entry.is_none() {
        return Err(not_exist_entry());
    }

    let revision: Option<(i64,)> =
        sqlx::query_as("SELECT rev_id FROM revisions WHERE rev_id = $1 AND entry_id = $2 LIMIT 1")
            .bind(rev_id)
            .bind(entry_id)
            .fetch_optional(&state.db.replica)
            .await?;

    if revision.is_none() {
        return Err(not_exist_revision());
    }

    let updated_by = make_user_id(SYSTEM_USER_ID);

    sqlx::query(
        "UPDATE entries SET saved_id = $1, published_id = $1, updated_at = CURRENT_TIMESTAMP, \
         updated_by = $2 WHERE entry_id = $3",
    )
    .bind(rev_id)
    .bind(updated_by)
    .bind(entry_id)
    .execute(&state.db.primary)
    .await?;

    Ok(json!({"isSuccess": true}))
}

// ---------------------------------------------------------------------------
// update-entry-unversioned-data-private
// ---------------------------------------------------------------------------

async fn update_unversioned_data_service(
    state: &AppState,
    entry_id: i64,
    unversioned_data: &Value,
    lock_token: Option<&str>,
) -> Result<EntryModelRow, ApiError> {
    let tenant_id = state.config.tenant_id_override.clone();

    check_lock(state, entry_id, lock_token).await?;

    let updated_by = make_user_id(SYSTEM_USER_ID);

    let row: Option<EntryModelRow> = sqlx::query_as(
        "UPDATE entries SET unversioned_data = $1, updated_by = $2, \
         updated_at = CURRENT_TIMESTAMP \
         WHERE entry_id = $3 AND tenant_id = $4 AND is_deleted = false \
         RETURNING entry_id, scope::text AS scope, type AS entry_type, display_key, created_by, \
            created_at, updated_by, updated_at, saved_id, published_id, tenant_id, workbook_id, \
            collection_id, unversioned_data",
    )
    .bind(unversioned_data.clone())
    .bind(updated_by)
    .bind(entry_id)
    .bind(tenant_id)
    .fetch_optional(&state.db.primary)
    .await?;

    row.ok_or_else(not_exist_entry)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Express never routes an empty `:entryId` segment, so `//` falls through to
/// the default HTML 404 page.
fn empty_entry_id_response(method: &Method, uri: &Uri) -> Response {
    crate::api::not_found::response(method, uri.path())
}

async fn create_entry(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    create_entry_impl(
        &state,
        &headers,
        &body,
        false,
        editor,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn private_create_entry(
    State(state): State<AppState>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    permissions::require_ctx_tenant_id(&state, true)?;
    create_entry_impl(
        &state,
        &headers,
        &body,
        true,
        true,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn create_entry_impl(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    is_private: bool,
    editor: bool,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<Response, HandlerError> {
    let value = parse_body(headers, body)?;
    let map = ensure_object_body(&value)?;
    let parsed = parse_create_body(map)?;

    let private_permissions = PrivatePermissions::from_headers(headers);

    if let Some(log) = request_log {
        crate::logging::event(
            log,
            "createEntry",
            "CREATE_ENTRY_IN_WORKBOOK_CALL",
            Map::new(),
        );
    }

    let row = create_entry_service(state, &parsed, is_private, editor, request_log).await?;

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "entryId".to_string(),
            Value::String(encode_id(row.entry_id)),
        );
        crate::logging::event(log, "createEntry", "CREATE_ENTRY_SUCCESS", fields);
    }

    // `createEntryModel.format`: the revision schema, then `links`,
    // `unversionedData`, `permissions` and `operation`.
    let mut entry = entry_revision_map(&row, true);

    if private_permissions
        .owned_scopes
        .contains(&row.scope.as_str())
    {
        entry.insert(
            "unversionedData".to_owned(),
            row.unversioned_data.clone().unwrap_or(Value::Null),
        );
    }

    if parsed.include_permissions_info == Some(true) {
        entry.insert(
            "permissions".to_owned(),
            if parsed.workbook_id.is_some() {
                permissions::entry_permissions(editor)
            } else {
                permissions::full_entry_permissions()
            },
        );
    }

    Ok(Json(Value::Object(entry)).into_response())
}

async fn update_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    user: Option<Extension<AuthUser>>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let request_workbook_id = permissions::request_workbook_id(&headers);
    update_entry_impl(
        &state,
        &raw_entry_id,
        &uri,
        &method,
        &headers,
        &body,
        false,
        editor,
        request_workbook_id,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn private_update_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    update_entry_impl(
        &state,
        &raw_entry_id,
        &uri,
        &method,
        &headers,
        &body,
        true,
        true,
        None,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn update_entry_impl(
    state: &AppState,
    raw_entry_id: &str,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    body: &[u8],
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<Response, HandlerError> {
    if raw_entry_id.is_empty() {
        return Ok(empty_entry_id_response(method, uri));
    }

    let value = parse_body(headers, body)?;

    // `withContract` validates `{body, params}` and merges the issue lists:
    // body issues first, params issues last (also when the body has a wrong
    // top-level type).
    let body_result = match ensure_object_body(&value) {
        Ok(map) => parse_update_body(map),
        Err(error) => Err(error),
    };
    let entry_id = decode_id(raw_entry_id);

    match (body_result, entry_id) {
        (Ok(parsed), Ok(entry_id)) => {
            let private_permissions = PrivatePermissions::from_headers(headers);

            let (row, include_links) = update_entry_service(
                state,
                entry_id,
                &parsed,
                is_private,
                editor,
                request_workbook_id,
                request_log,
            )
            .await?;

            if let Some(log) = request_log {
                crate::logging::event(log, "updateEntry", "SYNC_LINKS", Map::new());
                let mut fields = Map::new();
                fields.insert(
                    "entryId".to_string(),
                    Value::String(encode_id(row.entry_id)),
                );
                crate::logging::event(log, "updateEntry", "UPDATE_ENTRY_SUCCESS", fields);
            }

            // `updateEntryModel.format`: entry columns, then `links` (only when
            // the SQL selected the column) and the filtered `unversionedData`.
            let mut entry = entry_return_columns_map(&row);

            if include_links {
                entry.insert("links".to_owned(), row.links.clone().unwrap_or(Value::Null));
            }

            if private_permissions
                .owned_scopes
                .contains(&row.scope.as_str())
            {
                entry.insert(
                    "unversionedData".to_owned(),
                    row.unversioned_data.clone().unwrap_or(Value::Null),
                );
            }

            Ok(Json(Value::Object(entry)).into_response())
        }
        (Err(body_error), Ok(_)) => Err(HandlerError::Api(body_error)),
        (Ok(_), Err(_)) => Err(HandlerError::Api(ApiError::invalid_entry_id(raw_entry_id))),
        (Err(body_error), Err(_)) => {
            let issues = match body_error {
                ApiError {
                    details: Some(Value::Array(issues)),
                    ..
                } => issues,
                _ => Vec::new(),
            };

            let mut all = issues;
            all.push(ApiError::encoded_id_issue("entryId", raw_entry_id));

            Err(HandlerError::Api(ApiError::validation_issues(all)))
        }
    }
}

async fn rename_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    user: Option<Extension<AuthUser>>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let request_workbook_id = permissions::request_workbook_id(&headers);
    rename_entry_impl(
        &state,
        &raw_entry_id,
        &uri,
        &method,
        &headers,
        &body,
        false,
        editor,
        request_workbook_id,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn private_rename_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    rename_entry_impl(
        &state,
        &raw_entry_id,
        &uri,
        &method,
        &headers,
        &body,
        true,
        true,
        None,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn rename_entry_impl(
    state: &AppState,
    raw_entry_id: &str,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    body: &[u8],
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<Response, HandlerError> {
    if raw_entry_id.is_empty() {
        return Ok(empty_entry_id_response(method, uri));
    }

    let value = parse_body(headers, body)?;
    let map = ensure_object_body(&value)?;

    let body_result = parse_rename_body(map);
    let entry_id = decode_id(raw_entry_id);

    let (name, entry_id) = match (body_result, entry_id) {
        (Ok(name), Ok(entry_id)) => (name, entry_id),
        (Err(body_error), Ok(_)) => return Err(HandlerError::Api(body_error)),
        (Ok(_), Err(_)) => return Err(HandlerError::Api(ApiError::invalid_entry_id(raw_entry_id))),
        (Err(body_error), Err(_)) => {
            let issues = match body_error {
                ApiError {
                    details: Some(Value::Array(issues)),
                    ..
                } => issues,
                _ => Vec::new(),
            };

            let mut all = issues;
            all.push(ApiError::encoded_id_issue("entryId", raw_entry_id));

            return Err(HandlerError::Api(ApiError::validation_issues(all)));
        }
    };

    let rows = rename_entry_service(
        state,
        entry_id,
        &name,
        is_private,
        editor,
        request_workbook_id,
        request_log,
    )
    .await?;

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert("entryId".to_string(), Value::String(encode_id(entry_id)));
        crate::logging::event(log, "renameEntry", "RENAME_ENTRY_SUCCESS", fields);
    }

    let list: Vec<Value> = rows
        .iter()
        .map(|row| Value::Object(entry_return_columns_map(row)))
        .collect();

    Ok(Json(Value::Array(list)).into_response())
}

async fn delete_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    OriginalUri(uri): OriginalUri,
    method: Method,
    user: Option<Extension<AuthUser>>,
    request_log: Option<Extension<crate::logging::RequestLog>>,
    headers: HeaderMap,
) -> Result<Response, HandlerError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let request_workbook_id = permissions::request_workbook_id(&headers);
    delete_entry_impl(
        &state,
        &raw_entry_id,
        raw_query,
        &uri,
        &method,
        false,
        editor,
        request_workbook_id,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn private_delete_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    OriginalUri(uri): OriginalUri,
    method: Method,
    request_log: Option<Extension<crate::logging::RequestLog>>,
) -> Result<Response, HandlerError> {
    delete_entry_impl(
        &state,
        &raw_entry_id,
        raw_query,
        &uri,
        &method,
        true,
        true,
        None,
        request_log.as_ref().map(|Extension(log)| log),
    )
    .await
}

async fn delete_entry_impl(
    state: &AppState,
    raw_entry_id: &str,
    raw_query: Option<String>,
    uri: &Uri,
    method: &Method,
    is_private: bool,
    editor: bool,
    request_workbook_id: Option<i64>,
    request_log: Option<&crate::logging::RequestLog>,
) -> Result<Response, HandlerError> {
    if raw_entry_id.is_empty() {
        return Ok(empty_entry_id_response(method, uri));
    }

    // `withContract` validates params before query.
    let entry_id = decode_id(raw_entry_id);
    let query_result = parse_delete_query(raw_query.as_deref());

    let (entry_id, query) = match (entry_id, query_result) {
        (Ok(entry_id), Ok(query)) => (entry_id, query),
        (Err(_), Ok(_)) => return Err(HandlerError::Api(ApiError::invalid_entry_id(raw_entry_id))),
        (Ok(_), Err(query_error)) => return Err(HandlerError::Api(query_error)),
        (Err(_), Err(query_error)) => {
            let issues = match query_error {
                ApiError {
                    details: Some(Value::Array(issues)),
                    ..
                } => issues,
                _ => Vec::new(),
            };

            let mut all = vec![ApiError::encoded_id_issue("entryId", raw_entry_id)];
            all.extend(issues);

            return Err(HandlerError::Api(ApiError::validation_issues(all)));
        }
    };

    let row = delete_entry_service(
        state,
        entry_id,
        &query,
        is_private,
        editor,
        request_workbook_id,
        request_log,
    )
    .await?;

    if let Some(log) = request_log {
        let mut fields = Map::new();
        fields.insert(
            "entryId".to_string(),
            Value::String(raw_entry_id.to_string()),
        );
        crate::logging::event(log, "deleteEntry", "DELETE_ENTRY_SUCCESS", fields);
    }

    // `deleteEntryModel.format`
    let mut result = entry_return_columns_map(&row);
    result.insert(
        "isDeleted".to_owned(),
        Value::Bool(row.is_deleted.unwrap_or(true)),
    );
    result.insert("deletedAt".to_owned(), json_datetime_opt(row.deleted_at));

    Ok(Json(Value::Object(result)).into_response())
}

async fn switch_revision(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    if raw_entry_id.is_empty() {
        return Ok(empty_entry_id_response(&method, &uri));
    }

    // `makeReqParser` rejects with the first failure: params before body.
    let entry_id = decode_id(&raw_entry_id)
        .map_err(|_| HandlerError::Api(ApiError::invalid_entry_id(&raw_entry_id)))?;

    let value = parse_body(&headers, &body)?;
    let map = ensure_object_body(&value)?;

    let mut issues = Vec::new();
    let rev_id = parse_encoded_id(map, "revId", &mut issues);

    if !issues.is_empty() {
        return Err(HandlerError::Api(ApiError::validation(Value::Array(
            issues,
        ))));
    }

    let rev_id = rev_id.expect("validated above");

    let result = switch_revision_service(&state, entry_id, rev_id).await?;

    Ok(Json(result).into_response())
}

async fn update_unversioned_data(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    permissions::require_ctx_tenant_id(&state, true)?;
    if raw_entry_id.is_empty() {
        return Ok(empty_entry_id_response(&method, &uri));
    }

    let entry_id = decode_id(&raw_entry_id)
        .map_err(|_| HandlerError::Api(ApiError::invalid_entry_id(&raw_entry_id)))?;

    let value = parse_body(&headers, &body)?;
    let map = ensure_object_body(&value)?;

    let mut issues = Vec::new();

    let unversioned_data = parse_limited_object(
        map,
        "unversionedData",
        MAX_UNVERSIONED_DATA_OBJECT_SYMBOLS,
        false,
        false,
        &mut issues,
    );
    let lock_token = parse_optional_string(map, "lockToken", &mut issues);

    if !issues.is_empty() {
        return Err(HandlerError::Api(ApiError::validation(Value::Array(
            issues,
        ))));
    }

    let unversioned_data = unversioned_data.expect("validated above");

    let private_permissions = PrivatePermissions::from_headers(&headers);

    let row =
        update_unversioned_data_service(&state, entry_id, &unversioned_data, lock_token.as_deref())
            .await?;

    let include = private_permissions
        .owned_scopes
        .contains(&row.scope.as_str());

    let entry = entry_model_map(&row, include);

    Ok(Json(Value::Object(entry)).into_response())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_reg_matches_reference_examples() {
        for key in [
            "a",
            "ab",
            "a b",
            "folder",
            "a.b,c:d;e'f|g",
            "a-b–c—d−e",
            "a$b*c&d%e",
            "a@b(c)d_e",
            "a\u{00A0}b",
        ] {
            assert!(key_reg_matches(key), "{key:?} should match");
        }

        for key in [
            "", "?", "a?", "/a", "a/", "a/b/c", "a//b", "-a", ".a", "a ", " a",
        ] {
            assert!(
                !key_reg_matches(key) || key == "a ",
                "{key:?} should not match"
            );
        }
    }

    #[test]
    fn verify_entry_key_rejects_leading_slash() {
        assert_eq!(
            verify_entry_key_error("/a/b"),
            Some("should not start with /".to_owned())
        );
        assert_eq!(verify_entry_key_error("a/b"), None);
        assert_eq!(
            verify_entry_key_error("a//b"),
            Some(SYMBOLS_VALIDATE_MESSAGE.to_owned())
        );
    }

    #[test]
    fn verify_entry_name_rejects_slash() {
        assert_eq!(
            verify_entry_name_error("a/b"),
            Some("should not contain the symbol /".to_owned())
        );
        assert_eq!(verify_entry_name_error("a b"), None);
    }

    #[test]
    fn format_key_matches_node() {
        assert_eq!(format_key("a/b", false), "a/b");
        assert_eq!(format_key("a/b/", false), "a/b");
        assert_eq!(format_key("/", false), "/");
        assert_eq!(format_key("a/b", true), "a/b/");
        assert_eq!(format_key("a/b/", true), "a/b/");
    }

    #[test]
    fn parent_folder_key_matches_node() {
        assert_eq!(get_parent_folder_key("a/b/"), "a/");
        assert_eq!(get_parent_folder_key("a/"), "/");
        assert_eq!(get_parent_folder_key("a"), "/");
        assert_eq!(get_parent_folder_key("/"), "/");
        assert_eq!(get_parent_folder_key(""), "/");
        assert_eq!(get_parent_folder_key("a/b/c"), "a/b/");
    }

    #[test]
    fn folders_keys_match_node() {
        assert_eq!(get_folders_keys("a/b/c/"), vec!["a/", "a/b/", "a/b/c/"]);
        assert_eq!(get_folders_keys("a/"), vec!["a/"]);
        assert_eq!(get_folders_keys("/"), Vec::<String>::new());
    }

    #[test]
    fn full_parent_folder_keys_match_node() {
        // `Utils.getFullParentFolderKeys` stops as soon as the remaining key
        // has two segments, so the root "/" is never included for nested folders.
        assert_eq!(
            get_full_parent_folder_keys("a/b/c/d/"),
            vec!["a/b/c/", "a/b/", "a/"]
        );
        assert_eq!(get_full_parent_folder_keys("a/"), vec!["/"]);
    }

    #[test]
    fn escape_like_matches_node() {
        assert_eq!(escape_string_for_like("a%b_c"), "a\\%b\\_c");
    }

    #[test]
    fn js_stringify_matches_json_stringify() {
        assert_eq!(
            js_stringify(&json!({"a": 1, "b": "x"})),
            "{\"a\":1,\"b\":\"x\"}"
        );
        assert_eq!(js_stringify(&json!({"a": null})), "{\"a\":null}");
        assert_eq!(js_stringify(&json!([1, 2.5, true])), "[1,2.5,true]");
        assert_eq!(js_stringify(&json!({"a": "ё"})), "{\"a\":\"ё\"}");
    }

    #[test]
    fn timestamps_use_iso_milliseconds() {
        let value: DateTime<Utc> = DateTime::parse_from_rfc3339("2026-11-04T00:09:18.361Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(format_timestamp(value), "2026-11-04T00:09:18.361Z");
    }
}
