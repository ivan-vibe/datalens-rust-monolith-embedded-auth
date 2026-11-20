//! Bulk/navigation read endpoints of `src/controllers/entries/*`.
//!
//! Port targets (see the Node sources):
//!   GET  /v1/entries                                    get-entries (offset)
//!   POST /v1/get-entries                                get-entries v2 (cursor)
//!   POST /v1/check-entries-existence                    check-entries-existence
//!   POST /v1/get-entries-data                           get-entries-data
//!   POST /v1/get-entries-meta                           get-entries-meta
//!   POST /v1/get-entries-annotation                     get-entries-annotation
//!   POST /v1/get-entries-relations                      get-entries-relations
//!   GET  /v1/entries/{entry_id}/access-description      get-entry-access-description
//!   GET  /v1/entries/{entry_id}/revisions               get-revisions
//!   GET  /v1/entries/{entry_id}/relations               get-relations

use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions::{self, EntryAccess};
use crate::state::{AppState, PrivatePermissions};

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/entries", get(list_entries)),
        (
            "/v1/entries/{entry_id}/access-description",
            get(access_description),
        ),
        ("/v1/entries/{entry_id}/revisions", get(revisions)),
        ("/v1/entries/{entry_id}/relations", get(relations)),
        ("/v1/get-entries", post(get_entries_v2)),
        ("/v1/check-entries-existence", post(check_entries_existence)),
        ("/v1/get-entries-data", post(get_entries_data)),
        ("/v1/get-entries-meta", post(get_entries_meta)),
        ("/v1/get-entries-annotation", post(get_entries_annotation)),
        ("/v1/get-entries-relations", post(get_entries_relations)),
        // `/private/...` aliases of the same controllers (express registers the
        // same handler with `authPolicy: disabled`).
        ("/private/entries", get(list_entries)),
        ("/private/v1/get-entries", post(get_entries_v2)),
        ("/private/entries/{entry_id}/revisions", get(revisions)),
        ("/private/entries/{entry_id}/relations", get(relations)),
        (
            "/private/v1/get-entries-relations",
            post(get_entries_relations),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Constants copied from the Node sources
// ---------------------------------------------------------------------------

/// `EntryScope` (`src/db/models/new/entry/types.ts`), declaration order.
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

/// `ALLOWED_ENTRIES_SCOPE` (`src/const/common.ts`).
const ALLOWED_ENTRIES_SCOPES: [&str; 5] = ["dash", "widget", "dataset", "connection", "report"];

/// `RelationDirection` (`src/services/entry/actions/get-related-entries.ts`).
const RELATION_DIRECTIONS: [&str; 2] = ["parent", "child"];

/// `LinkDirection` (`src/services/new/entry/get-entries-relations/types.ts`).
const LINK_DIRECTIONS: [&str; 2] = ["from", "to"];

/// `DEFAULT_PAGE_SIZE` (`src/const/common.ts`) — only used by `getEntryRevisions`.
const DEFAULT_PAGE_SIZE: i64 = 1000;

/// `DEFAULT_QUERY_TIMEOUT` is applied per connection in `db.rs`.

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Node serializes `Date` as `toISOString()`: UTC, milliseconds, `Z` suffix.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn json_opt_string(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::String(text.clone()),
        None => Value::Null,
    }
}

fn json_opt_id(value: Option<i64>) -> Value {
    match encode_id_opt(value) {
        Some(text) => Value::String(text),
        None => Value::Null,
    }
}

/// `res.locals.login` resolution: `resolveSpecialTokens` defaults it to
/// `unknown`; with auth enabled it is the token `userId` with dots replaced by
/// dashes (unless it looks like an e-mail). `replaceDotsInLogin`.
fn resolve_login(user: &Option<Extension<AuthUser>>) -> String {
    let login = user
        .as_ref()
        .and_then(|Extension(user)| user.user.login.clone())
        .unwrap_or_else(|| "unknown".to_string());

    if login.contains('@') {
        login
    } else {
        login.replace('.', "-")
    }
}

/// `isTrueArg` of `src/utils/env-utils.ts` with `TRUE_FLAGS = ['1','true',true]`.
fn is_true_arg(value: &str) -> bool {
    matches!(value.to_lowercase().as_str(), "1" | "true")
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------------
// zod issue builders (zod v4 shapes, verified against zod@4.3.6)
// ---------------------------------------------------------------------------

pub(crate) fn issue_invalid_type(expected: &str, received: &str, path: Value) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

fn issue_invalid_value(path: Value, values: &[&str]) -> Value {
    let options = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join("|");

    json!({
        "code": "invalid_value",
        "values": values,
        "path": path,
        "message": format!("Invalid option: expected one of {options}"),
    })
}

fn issue_custom(path: Value, message: &str) -> Value {
    json!({
        "code": "custom",
        "path": path,
        "message": message,
    })
}

fn issue_too_small_number(field: &str, minimum: i64) -> Value {
    json!({
        "origin": "number",
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": [field],
        "message": format!("Too small: expected number to be >={minimum}"),
    })
}

fn issue_too_big_number(field: &str, maximum: i64) -> Value {
    json!({
        "origin": "number",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": [field],
        "message": format!("Too big: expected number to be <={maximum}"),
    })
}

fn issue_too_small_array(path: Value, minimum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": path,
        "message": format!("Too small: expected array to have >={minimum} items"),
    })
}

fn issue_too_big_array(path: Value, maximum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected array to have <={maximum} items"),
    })
}

/// `zc.encodedId()` union `zc.encodedIdArray()` — zod reports `invalid_union`
/// with the issues of both branches (verified against the reference).
fn rev_ids_union_issue(array_form: bool, items: &[String]) -> Value {
    let branch1 = if array_form {
        vec![
            issue_invalid_type("string", "array", json!([])),
            issue_too_small_array(json!([]), 13),
        ]
    } else {
        let raw = items.first().cloned().unwrap_or_default();
        let length = raw.chars().count();
        if length < 13 {
            vec![json!({
                "origin": "string",
                "code": "too_small",
                "minimum": 13,
                "inclusive": true,
                "exact": true,
                "path": [],
                "message": "Too small: expected string to have >=13 characters",
            })]
        } else if length > 13 {
            vec![json!({
                "origin": "string",
                "code": "too_big",
                "maximum": 13,
                "inclusive": true,
                "exact": true,
                "path": [],
                "message": "Too big: expected string to have <=13 characters",
            })]
        } else {
            vec![issue_custom(
                json!([]),
                &format!("id '{raw}' has incorrect format"),
            )]
        }
    };

    let branch2 = if array_form {
        let bad = items
            .iter()
            .find(|item| decode_id(item).is_err())
            .cloned()
            .unwrap_or_default();
        vec![issue_custom(
            json!([]),
            &format!("id '{bad}' has incorrect format"),
        )]
    } else {
        vec![issue_invalid_type("array", "string", json!([]))]
    };

    json!({
        "code": "invalid_union",
        "errors": [branch1, branch2],
        "path": ["revIds"],
        "message": "Invalid input",
    })
}

fn validation(issues: Vec<Value>) -> ApiError {
    ApiError::validation_issues(issues)
}

/// `ZodError#message` of zod v4 is `JSON.stringify(issues, null, 2)`.
fn zod_error_message(issues: &[Value]) -> String {
    serde_json::to_string_pretty(&Value::Array(issues.to_vec()))
        .unwrap_or_else(|_| "[]".to_string())
}

fn invalid_page_token(message: &str) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::BAD_REQUEST,
        Some("INVALID_PAGE_TOKEN"),
        message.to_string(),
    )
}

// ---------------------------------------------------------------------------
// express/qs compatible query-string parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QVal {
    Str(String),
    Arr(Vec<String>),
    Obj(Vec<(String, String)>),
}

impl QVal {
    /// Name zod uses for this value in `invalid_type` issues.
    pub(crate) fn received(&self) -> &'static str {
        match self {
            QVal::Str(_) => "string",
            QVal::Arr(_) => "array",
            QVal::Obj(_) => "object",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RawQuery {
    items: Vec<(String, QVal)>,
}

impl RawQuery {
    /// Port of express' default (`qs`) query parser for the shapes the API uses:
    /// repeated keys become arrays, `a[b]=c` becomes an object.
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        let mut query = RawQuery::default();
        let Some(raw) = raw else {
            return query;
        };

        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }

            let (raw_key, raw_value) = match pair.split_once('=') {
                Some((key, value)) => (key, value),
                None => (pair, ""),
            };

            let key = percent_decode(raw_key);
            let value = percent_decode(raw_value);

            match split_bracket(&key) {
                Some((base, inner)) => {
                    match query.items.iter().position(|(name, _)| name == &base) {
                        Some(index) => match &mut query.items[index].1 {
                            QVal::Obj(fields) => {
                                if let Some(slot) =
                                    fields.iter_mut().find(|(name, _)| name == &inner)
                                {
                                    slot.1 = value;
                                } else {
                                    fields.push((inner, value));
                                }
                            }
                            _ => {
                                query.items[index].1 = QVal::Obj(vec![(inner, value)]);
                            }
                        },
                        None => query.items.push((base, QVal::Obj(vec![(inner, value)]))),
                    }
                }
                None => match query.items.iter().position(|(name, _)| name == &key) {
                    Some(index) => match &mut query.items[index].1 {
                        QVal::Str(old) => {
                            let old = std::mem::take(old);
                            query.items[index].1 = QVal::Arr(vec![old, value]);
                        }
                        QVal::Arr(items) => items.push(value),
                        QVal::Obj(_) => {}
                    },
                    None => query.items.push((key, QVal::Str(value))),
                },
            }
        }

        query
    }

    pub(crate) fn get(&self, key: &str) -> Option<&QVal> {
        self.items
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

/// `a[b]` / `a[]` → `("a", Some("b"|""))`.
fn split_bracket(key: &str) -> Option<(String, String)> {
    if !key.ends_with(']') {
        return None;
    }

    let open = key.find('[')?;
    Some((
        key[..open].to_string(),
        key[open + 1..key.len() - 1].to_string(),
    ))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);

                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
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

/// `zc.queryArray()`: string → `[string]`, array → as-is, record → `Object.values`
/// (integer keys ascending, like JavaScript).
fn query_array(value: &QVal) -> Vec<String> {
    match value {
        QVal::Str(text) => vec![text.clone()],
        QVal::Arr(items) => items.clone(),
        QVal::Obj(fields) => {
            let mut fields = fields.clone();
            fields.sort_by_key(|(key, _)| key.parse::<u64>().unwrap_or(u64::MAX));
            fields.into_iter().map(|(_, value)| value).collect()
        }
    }
}

/// `zc.stringBoolean()`: `z.string().toLowerCase().transform(isTrueArg)`.
fn parse_string_boolean(value: &QVal, field: &str, issues: &mut Vec<Value>) -> Option<bool> {
    match value {
        QVal::Str(text) => Some(is_true_arg(text)),
        other => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!([field]),
            ));
            None
        }
    }
}

/// `parseInt(value, 10)` with JavaScript semantics (leading whitespace, sign,
/// digits, ignores the rest).
fn js_parse_int(text: &str) -> Option<f64> {
    let trimmed = text.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    let mut sign = 1.0f64;

    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        if bytes[index] == b'-' {
            sign = -1.0;
        }
        index += 1;
    }

    let mut digits = String::new();
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        digits.push(bytes[index] as char);
        index += 1;
    }

    if digits.is_empty() {
        return None;
    }

    digits.parse::<f64>().ok().map(|value| sign * value)
}

/// `zc.stringNumber({min, max})`.
fn parse_string_number(
    value: &QVal,
    field: &str,
    min: Option<i64>,
    max: Option<i64>,
    issues: &mut Vec<Value>,
) -> Option<i64> {
    let text = match value {
        QVal::Str(text) => text,
        other => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!([field]),
            ));
            return None;
        }
    };

    let Some(parsed) = js_parse_int(text) else {
        // `parseInt` returned NaN → the transform yields `undefined`, which the
        // piped `z.number()` rejects.
        issues.push(issue_invalid_type("number", "undefined", json!([field])));
        return None;
    };

    if let Some(minimum) = min {
        if parsed < minimum as f64 {
            issues.push(issue_too_small_number(field, minimum));
            return None;
        }
    }

    if let Some(maximum) = max {
        if parsed > maximum as f64 {
            issues.push(issue_too_big_number(field, maximum));
            return None;
        }
    }

    Some(parsed as i64)
}

// ---------------------------------------------------------------------------
// Cursor pagination (`src/utils/cursor-pagination.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorField {
    CreatedAt,
    SortName,
    EntryId,
}

/// SQL regex of `zc.stringSqlTimestampz()` (the `.` before the fraction is
/// unescaped in the original, so it matches any character).
const TIMESTAMPTZ_PATTERN: &str =
    "/^\\d{4}-\\d{2}-\\d{2}[ T]\\d{2}:\\d{2}:\\d{2}(?:.\\d{1,6})?[+-]\\d{2}(?::?\\d{2})?$/";

impl CursorField {
    fn sql(self) -> &'static str {
        match self {
            CursorField::CreatedAt => "e.created_at",
            CursorField::SortName => "e.sort_name",
            CursorField::EntryId => "e.entry_id",
        }
    }

    /// SQL type the text cursor value has to be cast to for the comparison.
    fn cast(self) -> &'static str {
        match self {
            CursorField::CreatedAt => "timestamptz",
            CursorField::SortName => "bytea",
            CursorField::EntryId => "bigint",
        }
    }

    fn validate(self, value: &str) -> Result<(), Value> {
        match self {
            CursorField::CreatedAt => {
                if matches_timestamptz(value) {
                    Ok(())
                } else {
                    Err(json!({
                        "origin": "string",
                        "code": "invalid_format",
                        "format": "regex",
                        "pattern": TIMESTAMPTZ_PATTERN,
                        "path": [],
                        "message": "Invalid timestamptz",
                    }))
                }
            }
            CursorField::SortName => Ok(()),
            CursorField::EntryId => {
                if is_valid_bigint(value) {
                    Ok(())
                } else {
                    Err(json!({
                        "code": "custom",
                        "path": [],
                        "message": "String must be a valid integer",
                    }))
                }
            }
        }
    }
}

fn matches_timestamptz(value: &str) -> bool {
    // `^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:.\d{1,6})?[+-]\d{2}(?::?\d{2})?$`
    //
    // The `(?:.\d{1,6})?` group is optional and its `.` is a wildcard, so the
    // regex engine backtracks: `...12+00` matches with the group *absent* (the
    // `+` belongs to the offset), while `...12.558569+00` matches with it
    // present. That is why the tail is tried both with and without a fraction.
    fn digits(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
        let end = start + count;
        if end > bytes.len() || !bytes[start..end].iter().all(u8::is_ascii_digit) {
            None
        } else {
            Some(end)
        }
    }

    fn offset(bytes: &[u8], index: usize) -> bool {
        if !matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
            return false;
        }

        let Some(mut index) = digits(bytes, index + 1, 2) else {
            return false;
        };

        if bytes.get(index) == Some(&b':') {
            return digits(bytes, index + 1, 2) == Some(bytes.len());
        }

        if index == bytes.len() {
            return true;
        }

        digits(bytes, index, 2) == Some(bytes.len())
    }

    let bytes = value.as_bytes();
    let Some(index) = digits(bytes, 0, 4) else {
        return false;
    };
    if bytes.get(index) != Some(&b'-') {
        return false;
    }
    let Some(index) = digits(bytes, index + 1, 2) else {
        return false;
    };
    if bytes.get(index) != Some(&b'-') {
        return false;
    }
    let Some(index) = digits(bytes, index + 1, 2) else {
        return false;
    };
    if !matches!(bytes.get(index), Some(b' ') | Some(b'T')) {
        return false;
    }
    let Some(index) = digits(bytes, index + 1, 2) else {
        return false;
    };
    if bytes.get(index) != Some(&b':') {
        return false;
    }
    let Some(index) = digits(bytes, index + 1, 2) else {
        return false;
    };
    if bytes.get(index) != Some(&b':') {
        return false;
    }
    let Some(index) = digits(bytes, index + 1, 2) else {
        return false;
    };

    if offset(bytes, index) {
        return true;
    }

    // Fractional seconds: one wildcard byte followed by 1..=6 digits.
    let mut end = index + 1;
    let mut count = 0;
    while count < 6 && bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
        count += 1;
    }

    count >= 1 && offset(bytes, end)
}

/// `zc.stringBigInt()`: `BigInt(value)` must not throw.
fn is_valid_bigint(value: &str) -> bool {
    let trimmed = value.trim();
    let text = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_prefix('-'))
        .unwrap_or(trimmed);

    if text.is_empty() {
        // `BigInt('')` is `0n`.
        return trimmed.is_empty() || trimmed == "+" || trimmed == "-";
    }

    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_hexdigit());
    }
    if let Some(rest) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        return !rest.is_empty() && rest.chars().all(|ch| ('0'..='7').contains(&ch));
    }
    if let Some(rest) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        return !rest.is_empty() && rest.chars().all(|ch| ch == '0' || ch == '1');
    }

    text.chars().all(|ch| ch.is_ascii_digit())
}

/// `JSON.stringify` of a string (enough for the values the paginator stores).
fn json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');

    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }

    out.push('"');
    out
}

/// `encodeCursorPageToken`: base64url of `JSON.stringify(values)`.
fn encode_cursor_token(values: &[String]) -> String {
    let payload = values
        .iter()
        .map(|value| json_quote(value))
        .collect::<Vec<_>>()
        .join(",");

    URL_SAFE_NO_PAD.encode(format!("[{payload}]").as_bytes())
}

/// `Buffer.from(token, 'base64url')`: lenient — unknown characters are ignored.
fn base64url_decode(text: &str) -> Vec<u8> {
    let mut cleaned = String::with_capacity(text.len());

    for ch in text.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' => cleaned.push(ch),
            '+' => cleaned.push('-'),
            '/' => cleaned.push('_'),
            _ => {}
        }
    }

    while cleaned.len() % 4 != 0 {
        cleaned.push('=');
    }

    URL_SAFE.decode(cleaned.as_bytes()).unwrap_or_default()
}

/// Node's `JSON.parse` error messages, approximated for the common cases.
fn json_parse_message(text: &str) -> String {
    if text.trim().is_empty() {
        return "Unexpected end of JSON input".to_string();
    }

    let snippet: String = text.chars().take(30).collect();

    format!(
        "Unexpected token '{}', {} is not valid JSON",
        "?",
        json_quote(&snippet)
    )
}

/// `decodeCursorPageToken`.
fn decode_cursor_token(fields: &[CursorField], page_token: &str) -> Result<Vec<String>, ApiError> {
    let payload = base64url_decode(page_token);
    let text = String::from_utf8_lossy(&payload).into_owned();

    let parsed: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return Err(invalid_page_token(&format!(
                "Invalid page token: {}",
                json_parse_message(&text)
            )));
        }
    };

    let Some(items) = parsed.as_array() else {
        return Err(invalid_page_token("Invalid page token"));
    };

    if items.len() != fields.len() {
        return Err(invalid_page_token("Invalid page token"));
    }

    let mut values = Vec::with_capacity(items.len());

    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str() else {
            let issue = issue_invalid_type("string", value_type_name(item), json!([]));
            return Err(invalid_page_token(&format!(
                "Invalid page token: {}",
                zod_error_message(&[issue])
            )));
        };

        if let Err(issue) = fields[index].validate(text) {
            return Err(invalid_page_token(&format!(
                "Invalid page token: {}",
                zod_error_message(&[issue])
            )));
        }

        values.push(text.to_string());
    }

    Ok(values)
}

// ---------------------------------------------------------------------------
// GET /v1/entries/{entryId}/access-description
// ---------------------------------------------------------------------------

/// `src/services/new/entry/get-entry-access-description.ts`.
const ACCESS_DESCRIPTION_SQL: &str = r#"
SELECT
    e.scope::text AS scope,
    r.data       AS data
FROM entries AS e
JOIN revisions AS r
    ON r.rev_id = CASE WHEN $2::text = 'published' THEN e.published_id ELSE e.saved_id END
WHERE e.entry_id = $1
  AND e.is_deleted = FALSE
  AND e.tenant_id = $3
LIMIT 1
"#;

#[derive(Debug, sqlx::FromRow)]
struct AccessDescriptionRow {
    scope: String,
    data: Option<Value>,
}

/// `entry-access-description-model.ts`.
fn format_access_description(row: &AccessDescriptionRow) -> Value {
    let mut access_description: Option<String> = None;

    if row.scope == "dash" {
        if let Some(Value::String(text)) = row
            .data
            .as_ref()
            .and_then(|data| data.get("accessDescription"))
        {
            if !text.is_empty() {
                access_description = Some(text.clone());
            }
        }
    }

    let mut body = Map::new();
    body.insert(
        "accessDescription".to_string(),
        match access_description {
            Some(text) => Value::String(text),
            None => Value::Null,
        },
    );

    Value::Object(body)
}

async fn access_description(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
) -> Result<Response, ApiError> {
    if raw_entry_id.is_empty() {
        return Ok(crate::api::not_found::response(&method, uri.path()));
    }

    let query = RawQuery::parse(uri.query());
    let mut issues = Vec::new();

    // `params: z.object({entryId: zc.encodedId()})` — always reports first.
    let entry_id = match decode_id(&raw_entry_id) {
        Ok(entry_id) => Some(entry_id),
        Err(_) => {
            issues.push(ApiError::encoded_id_issue("entryId", &raw_entry_id));
            None
        }
    };

    let branch = match query.get("branch") {
        None => "saved",
        Some(QVal::Str(value)) if value == "saved" => "saved",
        Some(QVal::Str(value)) if value == "published" => "published",
        Some(QVal::Str(_)) => {
            issues.push(issue_invalid_value(
                json!(["branch"]),
                &["saved", "published"],
            ));
            "saved"
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["branch"]),
            ));
            "saved"
        }
    };

    if !issues.is_empty() {
        return Err(validation(issues));
    }

    let entry_id = entry_id.expect("validated above");

    let row: Option<AccessDescriptionRow> = sqlx::query_as(ACCESS_DESCRIPTION_SQL)
        .bind(entry_id)
        .bind(branch)
        .bind(state.config.tenant_id_override.as_str())
        .fetch_optional(&state.db.replica)
        .await?;

    let row = row.ok_or_else(ApiError::not_exist_entry)?;

    Ok(Json(format_access_description(&row)).into_response())
}

// ---------------------------------------------------------------------------
// GET /v1/entries/{entryId}/revisions
// ---------------------------------------------------------------------------

const ENTRY_LOOKUP_SQL: &str = r#"
SELECT entry_id, workbook_id, collection_id, tenant_id
FROM entries
WHERE entry_id = $1
  AND is_deleted = FALSE
  AND ($2::text IS NULL OR tenant_id = $2)
LIMIT 1
"#;

#[derive(Debug, sqlx::FromRow)]
struct EntryLookupRow {
    #[allow(dead_code)]
    entry_id: i64,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    tenant_id: Option<String>,
}

const WORKBOOK_LOOKUP_SQL: &str = r#"
SELECT workbook_id
FROM workbooks
WHERE workbook_id = $1
  AND deleted_at IS NULL
  AND tenant_id = $2
LIMIT 1
"#;

/// Rows shared by the navigation list, `getEntryRevisions` and the cursor
/// variants (`RETURN_NAVIGATION_COLUMNS` + the cursor/revId extras).
#[derive(Debug, Clone, sqlx::FromRow)]
struct NavigationRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    meta: Option<Value>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_by: String,
    updated_at: DateTime<Utc>,
    annotation: Option<Value>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    hidden: bool,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    unversioned_data: Option<Value>,
    workbook_title: Option<String>,
    collection_title: Option<String>,
    is_favorite: bool,
    data: Option<Value>,
    links: Option<Value>,
    cursor0: Option<String>,
    cursor1: Option<String>,
    rev_id: Option<i64>,
}

const NAVIGATION_COLUMNS: &str = r#"
    e.entry_id        AS entry_id,
    e.scope::text     AS scope,
    e.type            AS entry_type,
    e.display_key     AS display_key,
    r.meta            AS meta,
    e.created_by      AS created_by,
    e.created_at      AS created_at,
    r.updated_by      AS updated_by,
    r.updated_at      AS updated_at,
    r.annotation      AS annotation,
    e.saved_id        AS saved_id,
    e.published_id    AS published_id,
    e.hidden          AS hidden,
    e.workbook_id     AS workbook_id,
    e.collection_id   AS collection_id,
    e.unversioned_data AS unversioned_data,
    w.title           AS workbook_title,
    c.title           AS collection_title,
    (sub.entity_id IS NOT NULL) AS is_favorite,
    r.data            AS data,
    r.links           AS links
"#;

/// `navigation-entry-model.ts` + the `entry-revisions-page-model.ts` extras.
fn format_navigation_entry(row: &NavigationRow) -> Map<String, Value> {
    let mut entry = Map::new();
    entry.insert(
        "entryId".to_string(),
        Value::String(encode_id(row.entry_id)),
    );
    entry.insert("scope".to_string(), Value::String(row.scope.clone()));
    entry.insert("type".to_string(), Value::String(row.entry_type.clone()));
    entry.insert("key".to_string(), json_opt_string(&row.display_key));
    entry.insert("meta".to_string(), row.meta.clone().unwrap_or(Value::Null));
    entry.insert(
        "createdBy".to_string(),
        Value::String(row.created_by.clone()),
    );
    entry.insert(
        "createdAt".to_string(),
        Value::String(format_timestamp(row.created_at)),
    );
    entry.insert(
        "updatedBy".to_string(),
        Value::String(row.updated_by.clone()),
    );
    entry.insert(
        "updatedAt".to_string(),
        Value::String(format_timestamp(row.updated_at)),
    );
    entry.insert(
        "annotation".to_string(),
        row.annotation.clone().unwrap_or(Value::Null),
    );
    entry.insert("savedId".to_string(), json_opt_id(row.saved_id));
    entry.insert("publishedId".to_string(), json_opt_id(row.published_id));
    entry.insert("hidden".to_string(), Value::Bool(row.hidden));
    entry.insert("workbookId".to_string(), json_opt_id(row.workbook_id));
    entry.insert("collectionId".to_string(), json_opt_id(row.collection_id));

    entry
}

const REVISIONS_SQL: &str = r#"
SELECT
    e.entry_id        AS entry_id,
    e.scope::text     AS scope,
    e.type            AS entry_type,
    e.display_key     AS display_key,
    r.meta            AS meta,
    e.created_by      AS created_by,
    e.created_at      AS created_at,
    r.updated_by      AS updated_by,
    r.updated_at      AS updated_at,
    r.annotation      AS annotation,
    e.saved_id        AS saved_id,
    e.published_id    AS published_id,
    e.hidden          AS hidden,
    e.workbook_id     AS workbook_id,
    e.collection_id   AS collection_id,
    NULL::jsonb       AS unversioned_data,
    NULL::text        AS workbook_title,
    NULL::text        AS collection_title,
    FALSE             AS is_favorite,
    NULL::jsonb       AS data,
    NULL::jsonb       AS links,
    r.updated_at::text AS cursor0,
    r.rev_id::text     AS cursor1,
    r.rev_id           AS rev_id
FROM entries AS e
JOIN revisions AS r ON r.entry_id = e.entry_id
WHERE e.entry_id = $1
  AND e.is_deleted = FALSE
  {rev_ids}
  {updated_after}
  {cursor}
ORDER BY r.updated_at DESC, r.rev_id DESC
LIMIT {limit}
"#;

async fn revisions(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
) -> Result<Response, ApiError> {
    if raw_entry_id.is_empty() {
        return Ok(crate::api::not_found::response(&method, uri.path()));
    }

    let query = RawQuery::parse(uri.query());

    // `makeReqParser` parses params and query concurrently and rejects with the
    // first failure, so a bad `:entryId` always wins.
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let mut issues = Vec::new();

    let page = match query.get("page") {
        None => None,
        Some(QVal::Str(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["page"]),
            ));
            None
        }
    };

    let page_size = match query.get("pageSize") {
        None => DEFAULT_PAGE_SIZE,
        Some(value) => {
            match parse_string_number(value, "pageSize", Some(1), Some(200), &mut issues) {
                Some(size) => size,
                None => DEFAULT_PAGE_SIZE,
            }
        }
    };

    let rev_ids = match query.get("revIds") {
        None => None,
        Some(value) => {
            let items = query_array(value);
            let array_form = matches!(value, QVal::Arr(_) | QVal::Obj(_))
                || uri
                    .query()
                    .map(|raw| raw.contains("revIds[") || raw.contains("revIds%5B"))
                    .unwrap_or(false);

            if items.len() > 1000 {
                issues.push(issue_too_big_array(json!(["revIds"]), 1000));
                None
            } else {
                let mut ids = Vec::with_capacity(items.len());
                let mut failed = false;
                for item in &items {
                    match decode_id(item) {
                        Ok(id) => ids.push(id),
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    issues.push(rev_ids_union_issue(array_form, &items));
                    None
                } else {
                    Some(ids)
                }
            }
        }
    };

    let updated_after = match query.get("updatedAfter") {
        None => None,
        Some(QVal::Str(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["updatedAfter"]),
            ));
            None
        }
    };

    if !issues.is_empty() {
        return Err(validation(issues));
    }

    // Private routes are not tenant-scoped (express skips the tenant filter).
    let tenant_filter = if uri.path().starts_with("/private") {
        None
    } else {
        Some(state.config.tenant_id_override.clone())
    };

    let entry: Option<EntryLookupRow> = sqlx::query_as(ENTRY_LOOKUP_SQL)
        .bind(entry_id)
        .bind(tenant_filter)
        .fetch_optional(&state.db.replica)
        .await?;

    let entry = entry.ok_or_else(ApiError::not_exist_entry)?;

    // The workbook check does not depend on the revisions query: run both
    // concurrently (one round-trip instead of two).
    let workbook_check = async {
        if entry.workbook_id.is_some() {
            let workbook: Option<(i64,)> = sqlx::query_as(WORKBOOK_LOOKUP_SQL)
                .bind(entry.workbook_id)
                .bind(state.config.tenant_id_override.as_str())
                .fetch_optional(&state.db.replica)
                .await?;

            if workbook.is_none() {
                return Err(ApiError::workbook_not_exists());
            }
        } else if entry.collection_id.is_some() {
            return Err(ApiError::collection_entry_unsupported_scope());
        } else if entry.tenant_id.as_deref() != Some(state.config.tenant_id_override.as_str()) {
            return Err(ApiError::not_match_together());
        }

        Ok::<(), ApiError>(())
    };

    let mut binds: Vec<Bind> = vec![Bind::I64(entry_id)];

    let rev_ids_sql = match &rev_ids {
        Some(ids) if !ids.is_empty() => {
            let placeholders = ids
                .iter()
                .map(|id| {
                    binds.push(Bind::I64(*id));
                    format!("${}", binds.len())
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND r.rev_id IN ({placeholders})")
        }
        _ => String::new(),
    };

    let updated_after_sql = match &updated_after {
        Some(value) => {
            binds.push(Bind::Str(value.clone()));
            format!("AND r.updated_at >= ${}::text::timestamptz", binds.len())
        }
        None => String::new(),
    };

    let cursor_sql = match &page {
        Some(token) => {
            let values =
                decode_cursor_token(&[CursorField::CreatedAt, CursorField::EntryId], token)?;
            binds.push(Bind::Str(values[0].clone()));
            let first = binds.len();
            binds.push(Bind::Str(values[1].clone()));
            let second = binds.len();

            format!(
                "AND (r.updated_at < ${first}::text::timestamptz OR (r.updated_at = ${first}::text::timestamptz AND r.rev_id < ${second}::text::bigint))"
            )
        }
        None => String::new(),
    };

    let sql = REVISIONS_SQL
        .replace("{rev_ids}", &rev_ids_sql)
        .replace("{updated_after}", &updated_after_sql)
        .replace("{cursor}", &cursor_sql)
        .replace("{limit}", &(page_size + 1).to_string());

    let mut request = sqlx::query_as::<_, NavigationRow>(&sql);
    for bind in binds {
        request = match bind {
            Bind::I64(value) => request.bind(value),
            Bind::Str(value) => request.bind(value),
        };
    }

    let (workbook_result, rows_result) =
        tokio::join!(workbook_check, request.fetch_all(&state.db.replica));
    workbook_result?;
    let mut rows = rows_result?;

    let mut next_page_token: Option<String> = None;

    if rows.len() as i64 > page_size {
        let last = &rows[page_size as usize - 1];
        next_page_token = Some(encode_cursor_token(&[
            last.cursor0.clone().unwrap_or_default(),
            last.cursor1.clone().unwrap_or_default(),
        ]));
        rows.truncate(page_size as usize);
    }

    let entries: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut entry = format_navigation_entry(row);
            entry.insert("revId".to_string(), json_opt_id(row.rev_id));
            Value::Object(entry)
        })
        .collect();

    let mut body = Map::new();
    if let Some(token) = next_page_token {
        body.insert("nextPageToken".to_string(), Value::String(token));
    }
    body.insert("entries".to_string(), Value::Array(entries));

    Ok(Json(Value::Object(body)).into_response())
}

// ---------------------------------------------------------------------------
// GET /v1/entries/{entryId}/relations
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct RelationRow {
    entry_id: i64,
    key: Option<String>,
    scope: String,
    entry_type: String,
    created_at: DateTime<Utc>,
    is_public: bool,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    depth: i32,
    meta: Option<Value>,
    links: Option<Value>,
}

#[derive(Debug, sqlx::FromRow)]
struct RelatedIdRow {
    entry_id: i64,
    depth: i32,
}

/// JS truthiness of a JSON value (`relation.meta ?? null`).
fn null_or(value: &Option<Value>) -> Value {
    value.clone().unwrap_or(Value::Null)
}

async fn relations(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, ApiError> {
    if raw_entry_id.is_empty() {
        return Ok(crate::api::not_found::response(&method, uri.path()));
    }

    let query = RawQuery::parse(uri.query());

    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let mut issues = Vec::new();

    let direction = match query.get("direction") {
        None => "parent".to_string(),
        Some(QVal::Str(value)) if RELATION_DIRECTIONS.contains(&value.as_str()) => value.clone(),
        Some(QVal::Str(_)) => {
            issues.push(issue_invalid_value(
                json!(["direction"]),
                &RELATION_DIRECTIONS,
            ));
            "parent".to_string()
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["direction"]),
            ));
            "parent".to_string()
        }
    };

    let include_permissions_info = match query.get("includePermissionsInfo") {
        None => false,
        Some(value) => {
            parse_string_boolean(value, "includePermissionsInfo", &mut issues).unwrap_or(false)
        }
    };

    let scope = match query.get("scope") {
        None => None,
        Some(QVal::Str(value)) if ALLOWED_ENTRIES_SCOPES.contains(&value.as_str()) => {
            Some(value.clone())
        }
        Some(QVal::Str(_)) => {
            issues.push(issue_invalid_value(
                json!(["scope"]),
                &ALLOWED_ENTRIES_SCOPES,
            ));
            None
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["scope"]),
            ));
            None
        }
    };

    let page = match query.get("page") {
        None => None,
        Some(value) => parse_string_number(value, "page", Some(0), None, &mut issues),
    };

    let page_size = match query.get("pageSize") {
        None => None,
        Some(value) => parse_string_number(value, "pageSize", Some(1), Some(200), &mut issues),
    };

    if !issues.is_empty() {
        return Err(validation(issues));
    }

    let tenant_filter = if uri.path().starts_with("/private") {
        None
    } else {
        Some(state.config.tenant_id_override.clone())
    };

    let entry: Option<EntryLookupRow> = sqlx::query_as(ENTRY_LOOKUP_SQL)
        .bind(entry_id)
        .bind(tenant_filter)
        .fetch_optional(&state.db.replica)
        .await?;

    let entry = entry.ok_or_else(ApiError::not_exist_entry)?;

    let is_private_route = uri.path().starts_with("/private");
    if !is_private_route {
        permissions::check_workbook_isolation(&headers, entry.workbook_id, entry.collection_id)?;
    }
    let editor = is_private_route
        || permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));

    // `getRelatedEntries`
    let end_to_start = direction == "parent";

    let mut binds: Vec<Bind> = Vec::new();
    binds.push(Bind::I64(entry_id));
    let first_placeholder = format!("${}", binds.len());

    let select_col = if end_to_start { "to_id" } else { "from_id" };

    let scope_sql_recursive = match &scope {
        Some(value) => {
            binds.push(Bind::Str(value.clone()));
            format!("AND entries.scope::text = ${}", binds.len())
        }
        None => String::new(),
    };

    let recursive_sql = if end_to_start {
        format!(
            r#"
WITH RECURSIVE related_entries AS (
    SELECT links.from_id AS from_id, links.to_id AS to_id, 1 AS depth
    FROM links
    INNER JOIN entries ON entries.entry_id = links.to_id
    WHERE entries.is_deleted = FALSE
      AND links.from_id IN ({first_placeholder})
    UNION
    SELECT l.from_id AS from_id, l.to_id AS to_id, related_entries.depth + 1 AS depth
    FROM links AS l
    INNER JOIN related_entries ON related_entries.to_id = l.from_id
    INNER JOIN entries ON entries.entry_id = l.to_id
    WHERE entries.is_deleted = FALSE
      AND related_entries.depth < 5
      {scope_sql_recursive}
)
SELECT {select_col} AS entry_id, depth FROM related_entries
"#
        )
    } else {
        format!(
            r#"
WITH RECURSIVE related_entries AS (
    SELECT links.from_id AS from_id, links.to_id AS to_id, 1 AS depth
    FROM links
    INNER JOIN entries ON entries.entry_id = links.from_id
    WHERE entries.is_deleted = FALSE
      AND links.to_id IN ({first_placeholder})
    UNION
    SELECT l.from_id AS from_id, l.to_id AS to_id, related_entries.depth + 1 AS depth
    FROM links AS l
    INNER JOIN related_entries ON related_entries.from_id = l.to_id
    INNER JOIN entries ON entries.entry_id = l.from_id
    WHERE entries.is_deleted = FALSE
      AND related_entries.depth < 5
      {scope_sql_recursive}
)
SELECT {select_col} AS entry_id, depth FROM related_entries
"#
        )
    };

    let related_ids: Vec<RelatedIdRow> = {
        let mut request = sqlx::query_as::<_, RelatedIdRow>(&recursive_sql);
        for bind in &binds {
            request = match bind {
                Bind::I64(value) => request.bind(*value),
                Bind::Str(value) => request.bind(value.clone()),
            };
        }
        request.fetch_all(&state.db.replica).await?
    };

    let mut depth_map: Vec<(i64, i32)> = Vec::new();
    for row in &related_ids {
        match depth_map.iter_mut().find(|(id, _)| *id == row.entry_id) {
            Some((_, depth)) => {
                if row.depth < *depth {
                    *depth = row.depth;
                }
            }
            None => depth_map.push((row.entry_id, row.depth)),
        }
    }

    let mut relation_rows: Vec<RelationRow> = Vec::new();

    if !depth_map.is_empty() {
        let mut binds: Vec<Bind> = Vec::new();
        binds.push(Bind::Str(
            state.config.tenant_id_override.as_str().to_string(),
        ));
        let tenant_placeholder = format!("${}", binds.len());

        let id_placeholders = depth_map
            .iter()
            .map(|(id, _)| {
                binds.push(Bind::I64(*id));
                format!("${}", binds.len())
            })
            .collect::<Vec<_>>()
            .join(", ");

        let scope_sql = match &scope {
            Some(value) => {
                binds.push(Bind::Str(value.clone()));
                format!("AND e.scope::text = ${}", binds.len())
            }
            None => String::new(),
        };

        let mut sql = format!(
            r#"
SELECT
    e.entry_id      AS entry_id,
    e.display_key   AS key,
    e.scope::text   AS scope,
    e.type          AS entry_type,
    e.created_at    AS created_at,
    e.public        AS is_public,
    e.tenant_id     AS tenant_id,
    e.workbook_id   AS workbook_id,
    e.collection_id AS collection_id,
    0::int4         AS depth,
    r.meta          AS meta,
    r.links         AS links
FROM entries AS e
JOIN revisions AS r ON r.rev_id = COALESCE(e.published_id, e.saved_id)
WHERE e.entry_id IN ({id_placeholders})
  AND e.tenant_id = {tenant_placeholder}
  {scope_sql}
ORDER BY e.created_at
"#
        );

        if let Some(size) = page_size {
            sql.push_str(&format!(" LIMIT {size}"));
            if let Some(page) = page {
                if page > 0 {
                    sql.push_str(&format!(" OFFSET {}", size.saturating_mul(page)));
                }
            }
        }

        let mut request = sqlx::query_as::<_, RelationRow>(&sql);
        for bind in binds {
            request = match bind {
                Bind::I64(value) => request.bind(value),
                Bind::Str(value) => request.bind(value),
            };
        }
        relation_rows = request.fetch_all(&state.db.replica).await?;
    }

    let mut related: Vec<RelationRow> = relation_rows
        .into_iter()
        .map(|mut row| {
            row.depth = depth_map
                .iter()
                .find(|(id, _)| *id == row.entry_id)
                .map(|(_, depth)| *depth)
                .unwrap_or(1);
            row
        })
        .collect();

    // `relations.filter((item) => item.tenantId === entry.tenantId)`
    related.retain(|item| item.tenant_id == entry.tenant_id);

    // Permission filtering: with the access service disabled every branch below
    // resolves to "allowed", except collection entries which throw.
    let mut filtered: Vec<RelationRow> = Vec::new();

    if let Some(workbook_id) = entry.workbook_id {
        let workbook: Option<(i64,)> = sqlx::query_as(WORKBOOK_LOOKUP_SQL)
            .bind(workbook_id)
            .bind(state.config.tenant_id_override.as_str())
            .fetch_optional(&state.db.replica)
            .await?;

        if workbook.is_none() {
            return Err(ApiError::workbook_not_exists());
        }

        let has_collection_entries = related
            .iter()
            .any(|item| item.collection_id.is_some() && item.workbook_id != Some(workbook_id));

        if has_collection_entries {
            return Err(ApiError::collection_entry_unsupported_scope());
        }

        filtered = related
            .into_iter()
            .filter(|item| item.workbook_id == Some(workbook_id))
            .collect();
    } else if entry.collection_id.is_some() {
        return Err(ApiError::collection_entry_unsupported_scope());
    } else {
        filtered = related;
    }

    let mut next_page_token = None;
    if let (Some(page), Some(page_size)) = (page, page_size) {
        if filtered.len() as i64 == page_size {
            next_page_token = Some((page + 1).to_string());
        }
    }

    let relation_items: Vec<Value> = filtered
        .iter()
        .map(|item| {
            let mut relation = Map::new();
            relation.insert(
                "entryId".to_string(),
                Value::String(encode_id(item.entry_id)),
            );
            relation.insert("key".to_string(), json_opt_string(&item.key));
            relation.insert("scope".to_string(), Value::String(item.scope.clone()));
            relation.insert("type".to_string(), Value::String(item.entry_type.clone()));
            relation.insert(
                "createdAt".to_string(),
                Value::String(format_timestamp(item.created_at)),
            );
            relation.insert("public".to_string(), Value::Bool(item.is_public));
            relation.insert("tenantId".to_string(), json_opt_string(&item.tenant_id));
            relation.insert("workbookId".to_string(), json_opt_id(item.workbook_id));
            relation.insert("collectionId".to_string(), json_opt_id(item.collection_id));
            relation.insert("depth".to_string(), json!(item.depth));
            relation.insert("meta".to_string(), null_or(&item.meta));
            relation.insert("links".to_string(), null_or(&item.links));
            relation.insert("isLocked".to_string(), Value::Bool(false));

            if include_permissions_info {
                relation.insert(
                    "permissions".to_string(),
                    if item.workbook_id.is_some() {
                        permissions::entry_permissions(editor)
                    } else {
                        permissions::full_entry_permissions()
                    },
                );
            }

            Value::Object(relation)
        })
        .collect();

    if page.is_some() && page_size.is_some() {
        let mut body = Map::new();
        body.insert("relations".to_string(), Value::Array(relation_items));
        if let Some(token) = next_page_token {
            body.insert("nextPageToken".to_string(), Value::String(token));
        }
        return Ok(Json(Value::Object(body)).into_response());
    }

    Ok(Json(Value::Array(relation_items)).into_response())
}

// ---------------------------------------------------------------------------
// SQL bind helper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Bind {
    I64(i64),
    Str(String),
}

impl Bind {
    fn placeholder(binds: &mut Vec<Bind>, value: Bind) -> String {
        binds.push(value);
        format!("${}", binds.len())
    }
}

// ---------------------------------------------------------------------------
// Navigation list: GET /v1/entries (offset) and POST /v1/get-entries (cursor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderField {
    CreatedAt,
    Name,
}

#[derive(Debug, Clone, Copy)]
struct OrderSpec {
    field: OrderField,
    ascending: bool,
}

#[derive(Debug, Clone)]
struct ListQuery {
    ids: Option<Vec<i64>>,
    scope: Option<String>,
    types: Option<Vec<String>>,
    created_by: Option<Vec<String>>,
    order_by: Option<OrderSpec>,
    meta: Option<Vec<(String, String)>>,
    filters_name: Option<String>,
    page_size: i64,
    page: i64,
    page_token: Option<String>,
    include_permissions_info: bool,
    ignore_workbook_entries: bool,
    ignore_shared_entries: bool,
    include_data: bool,
    include_links: bool,
    exclude_locked: bool,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            ids: None,
            scope: None,
            types: None,
            created_by: None,
            order_by: None,
            meta: None,
            filters_name: None,
            // `pageSize = 100`, `page = 0` defaults of `Navigation.getEntries`.
            page_size: 100,
            page: 0,
            page_token: None,
            include_permissions_info: false,
            ignore_workbook_entries: false,
            ignore_shared_entries: false,
            include_data: false,
            include_links: false,
            exclude_locked: false,
        }
    }
}

/// `orderBySchema` of `get-entries.ts`.
fn parse_order_by(value: &QVal, field_name: &str, issues: &mut Vec<Value>) -> Option<OrderSpec> {
    match value {
        QVal::Obj(fields) => {
            let field_value = fields
                .iter()
                .find(|(name, _)| name == "field")
                .map(|(_, value)| value.as_str());

            let field = match field_value {
                Some("createdAt") => Some(OrderField::CreatedAt),
                Some("name") => Some(OrderField::Name),
                _ => {
                    issues.push(issue_invalid_value(
                        json!([field_name, "field"]),
                        &["createdAt", "name"],
                    ));
                    None
                }
            };

            let direction_value = fields
                .iter()
                .find(|(name, _)| name == "direction")
                .map(|(_, value)| value.as_str());

            let ascending = match direction_value {
                Some("asc") => Some(true),
                Some("desc") => Some(false),
                _ => {
                    issues.push(issue_invalid_value(
                        json!([field_name, "direction"]),
                        &["asc", "desc"],
                    ));
                    None
                }
            };

            match (field, ascending) {
                (Some(field), Some(ascending)) => Some(OrderSpec { field, ascending }),
                _ => None,
            }
        }
        other => {
            issues.push(issue_invalid_type(
                "object",
                other.received(),
                json!([field_name]),
            ));
            None
        }
    }
}

fn parse_list_query(raw: Option<&str>) -> Result<ListQuery, ApiError> {
    let query = RawQuery::parse(raw);
    let mut issues = Vec::new();
    let mut result = ListQuery::default();

    // `ids`
    if let Some(value) = query.get("ids") {
        let items = query_array(value);

        if items.len() > 1000 {
            issues.push(issue_too_big_array(json!(["ids"]), 1000));
        } else {
            let mut decoded = Vec::new();
            for item in &items {
                if let Ok(id) = decode_id(item) {
                    decoded.push(id);
                }
            }
            result.ids = Some(decoded);
        }
    }

    // `scope`
    if let Some(value) = query.get("scope") {
        match value {
            QVal::Str(text) if ENTRY_SCOPES.contains(&text.as_str()) => {
                result.scope = Some(text.clone());
            }
            QVal::Str(_) => issues.push(issue_invalid_value(json!(["scope"]), &ENTRY_SCOPES)),
            other => issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["scope"]),
            )),
        }
    }

    // `type`
    if let Some(value) = query.get("type") {
        let items = query_array(value);

        if items.is_empty() {
            issues.push(issue_too_small_array(json!(["type"]), 1));
        } else if items.len() > 10 {
            issues.push(issue_too_big_array(json!(["type"]), 10));
        } else {
            result.types = Some(items);
        }
    }

    // `createdBy: z.union([z.string(), z.array(z.string())])`
    if let Some(value) = query.get("createdBy") {
        match value {
            QVal::Str(text) => result.created_by = Some(vec![text.clone()]),
            QVal::Arr(items) => result.created_by = Some(items.clone()),
            other => issues.push(issue_invalid_type(
                "string",
                other.received(),
                json!(["createdBy"]),
            )),
        }
    }

    // `orderBy`
    if let Some(value) = query.get("orderBy") {
        result.order_by = parse_order_by(value, "orderBy", &mut issues);
    }

    // `meta`
    if let Some(value) = query.get("meta") {
        match value {
            QVal::Obj(fields) => result.meta = Some(fields.clone()),
            other => issues.push(issue_invalid_type(
                "record",
                other.received(),
                json!(["meta"]),
            )),
        }
    }

    // `filters`
    if let Some(value) = query.get("filters") {
        match value {
            QVal::Obj(fields) => {
                if let Some((_, name)) = fields.iter().find(|(key, _)| key == "name") {
                    result.filters_name = Some(name.clone());
                }
            }
            other => issues.push(issue_invalid_type(
                "object",
                other.received(),
                json!(["filters"]),
            )),
        }
    }

    // `pageSize`
    if let Some(value) = query.get("pageSize") {
        if let Some(size) = parse_string_number(value, "pageSize", Some(1), Some(200), &mut issues)
        {
            result.page_size = size;
        }
    }

    // boolean flags, in schema declaration order
    if let Some(value) = query.get("includePermissionsInfo") {
        result.include_permissions_info =
            parse_string_boolean(value, "includePermissionsInfo", &mut issues).unwrap_or(false);
    }
    if let Some(value) = query.get("ignoreWorkbookEntries") {
        result.ignore_workbook_entries =
            parse_string_boolean(value, "ignoreWorkbookEntries", &mut issues).unwrap_or(false);
    }
    if let Some(value) = query.get("ignoreSharedEntries") {
        result.ignore_shared_entries =
            parse_string_boolean(value, "ignoreSharedEntries", &mut issues).unwrap_or(false);
    }
    if let Some(value) = query.get("includeData") {
        result.include_data =
            parse_string_boolean(value, "includeData", &mut issues).unwrap_or(false);
    }
    if let Some(value) = query.get("includeLinks") {
        result.include_links =
            parse_string_boolean(value, "includeLinks", &mut issues).unwrap_or(false);
    }
    if let Some(value) = query.get("excludeLocked") {
        result.exclude_locked =
            parse_string_boolean(value, "excludeLocked", &mut issues).unwrap_or(false);
    }

    // `page`
    if let Some(value) = query.get("page") {
        if let Some(page) = parse_string_number(value, "page", Some(0), None, &mut issues) {
            result.page = page;
        }
    }

    // `.refine(idsOrScopeRefine, …)`
    if issues.is_empty() && result.ids.is_none() && result.scope.is_none() {
        issues.push(issue_custom(
            json!([]),
            "Either \"ids\" or \"scope\" must be provided.",
        ));
    }

    if issues.is_empty() {
        Ok(result)
    } else {
        Err(validation(issues))
    }
}

fn body_bool(map: &Map<String, Value>, key: &str, issues: &mut Vec<Value>) -> bool {
    match map.get(key) {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            issues.push(issue_invalid_type(
                "boolean",
                value_type_name(other),
                json!([key]),
            ));
            false
        }
    }
}

fn body_number(
    map: &Map<String, Value>,
    key: &str,
    min: i64,
    max: Option<i64>,
    fallback: Option<i64>,
    issues: &mut Vec<Value>,
) -> Option<i64> {
    let value = map.get(key)?;

    let Value::Number(number) = value else {
        issues.push(issue_invalid_type(
            "number",
            value_type_name(value),
            json!([key]),
        ));
        return fallback;
    };

    let Some(number) = number.as_i64() else {
        issues.push(issue_invalid_type("int", "number", json!([key])));
        return fallback;
    };

    if number < min {
        issues.push(issue_too_small_number(key, min));
        return fallback;
    }

    if let Some(maximum) = max {
        if number > maximum {
            issues.push(issue_too_big_number(key, maximum));
            return fallback;
        }
    }

    Some(number)
}

/// `requestV2Schema.body` of `get-entries.ts`.
fn parse_list_body(body: &Value) -> Result<ListQuery, ApiError> {
    let Value::Object(map) = body else {
        return Err(validation(vec![issue_invalid_type(
            "object",
            value_type_name(body),
            json!([]),
        )]));
    };

    let mut issues = Vec::new();
    let mut result = ListQuery::default();

    // `ids: zc.encodedIdArraySafe({min: 0, max: 1000}).optional()`
    if let Some(value) = map.get("ids") {
        match value {
            Value::Array(items) => {
                if items.len() > 1000 {
                    issues.push(issue_too_big_array(json!(["ids"]), 1000));
                } else {
                    let mut decoded = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        match item {
                            Value::String(text) => {
                                if let Ok(id) = decode_id(text) {
                                    decoded.push(id);
                                }
                            }
                            other => issues.push(issue_invalid_type(
                                "string",
                                value_type_name(other),
                                json!(["ids", index]),
                            )),
                        }
                    }
                    result.ids = Some(decoded);
                }
            }
            other => issues.push(issue_invalid_type(
                "array",
                value_type_name(other),
                json!(["ids"]),
            )),
        }
    }

    // `scope: z.enum(EntryScope).optional()`
    if let Some(value) = map.get("scope") {
        match value {
            Value::String(text) if ENTRY_SCOPES.contains(&text.as_str()) => {
                result.scope = Some(text.clone());
            }
            Value::String(_) => issues.push(issue_invalid_value(json!(["scope"]), &ENTRY_SCOPES)),
            other => issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["scope"]),
            )),
        }
    }

    // `type: z.union([z.string(), z.array(z.string()).min(1).max(10)]).optional()`
    if let Some(value) = map.get("type") {
        match value {
            Value::String(text) => result.types = Some(vec![text.clone()]),
            Value::Array(items) => {
                if items.is_empty() {
                    issues.push(issue_too_small_array(json!(["type"]), 1));
                } else if items.len() > 10 {
                    issues.push(issue_too_big_array(json!(["type"]), 10));
                } else {
                    let mut types = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        match item {
                            Value::String(text) => types.push(text.clone()),
                            other => issues.push(issue_invalid_type(
                                "string",
                                value_type_name(other),
                                json!(["type", index]),
                            )),
                        }
                    }
                    result.types = Some(types);
                }
            }
            other => issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["type"]),
            )),
        }
    }

    // `createdBy: z.array(z.string()).max(1000).optional()`
    if let Some(value) = map.get("createdBy") {
        match value {
            Value::Array(items) => {
                if items.len() > 1000 {
                    issues.push(issue_too_big_array(json!(["createdBy"]), 1000));
                } else {
                    let mut creators = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        match item {
                            Value::String(text) => creators.push(text.clone()),
                            other => issues.push(issue_invalid_type(
                                "string",
                                value_type_name(other),
                                json!(["createdBy", index]),
                            )),
                        }
                    }
                    result.created_by = Some(creators);
                }
            }
            other => issues.push(issue_invalid_type(
                "array",
                value_type_name(other),
                json!(["createdBy"]),
            )),
        }
    }

    // `orderBy`
    if let Some(value) = map.get("orderBy") {
        match value {
            Value::Object(fields) => {
                let field_value = fields.get("field").and_then(Value::as_str);
                let field = match field_value {
                    Some("createdAt") => Some(OrderField::CreatedAt),
                    Some("name") => Some(OrderField::Name),
                    _ => {
                        issues.push(issue_invalid_value(
                            json!(["orderBy", "field"]),
                            &["createdAt", "name"],
                        ));
                        None
                    }
                };

                let direction_value = fields.get("direction").and_then(Value::as_str);
                let ascending = match direction_value {
                    Some("asc") => Some(true),
                    Some("desc") => Some(false),
                    _ => {
                        issues.push(issue_invalid_value(
                            json!(["orderBy", "direction"]),
                            &["asc", "desc"],
                        ));
                        None
                    }
                };

                if let (Some(field), Some(ascending)) = (field, ascending) {
                    result.order_by = Some(OrderSpec { field, ascending });
                }
            }
            other => issues.push(issue_invalid_type(
                "object",
                value_type_name(other),
                json!(["orderBy"]),
            )),
        }
    }

    // `meta: z.record(z.string(), z.union([z.string(), z.array(z.string())]))`
    if let Some(value) = map.get("meta") {
        match value {
            Value::Object(fields) => {
                let mut filters = Vec::new();
                for (key, value) in fields {
                    match value {
                        Value::String(text) => filters.push((key.clone(), text.clone())),
                        Value::Array(items) => {
                            // A JSON array makes the `?::text` bind fail in
                            // PostgreSQL exactly like the Node implementation.
                            let first = items
                                .first()
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            filters.push((key.clone(), first));
                        }
                        other => issues.push(issue_invalid_type(
                            "string",
                            value_type_name(other),
                            json!(["meta", key]),
                        )),
                    }
                }
                result.meta = Some(filters);
            }
            other => issues.push(issue_invalid_type(
                "record",
                value_type_name(other),
                json!(["meta"]),
            )),
        }
    }

    // `filters: z.object({name: z.string().optional()}).optional()`
    if let Some(value) = map.get("filters") {
        match value {
            Value::Object(fields) => match fields.get("name") {
                None => {}
                Some(Value::String(name)) => result.filters_name = Some(name.clone()),
                Some(other) => issues.push(issue_invalid_type(
                    "string",
                    value_type_name(other),
                    json!(["filters", "name"]),
                )),
            },
            other => issues.push(issue_invalid_type(
                "object",
                value_type_name(other),
                json!(["filters"]),
            )),
        }
    }

    // `pageSize: z.number().int().min(1).max(200).optional()`
    if let Some(size) = body_number(map, "pageSize", 1, Some(200), Some(100), &mut issues) {
        result.page_size = size;
    }

    result.include_permissions_info = body_bool(map, "includePermissionsInfo", &mut issues);
    result.ignore_workbook_entries = body_bool(map, "ignoreWorkbookEntries", &mut issues);
    result.ignore_shared_entries = body_bool(map, "ignoreSharedEntries", &mut issues);
    result.include_data = body_bool(map, "includeData", &mut issues);
    result.include_links = body_bool(map, "includeLinks", &mut issues);
    result.exclude_locked = body_bool(map, "excludeLocked", &mut issues);

    // `pageToken: z.string().optional()`
    if let Some(value) = map.get("pageToken") {
        match value {
            Value::String(token) => result.page_token = Some(token.clone()),
            other => issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["pageToken"]),
            )),
        }
    }

    if issues.is_empty() && result.ids.is_none() && result.scope.is_none() {
        issues.push(issue_custom(
            json!([]),
            "Either \"ids\" or \"scope\" must be provided.",
        ));
    }

    if issues.is_empty() {
        Ok(result)
    } else {
        Err(validation(issues))
    }
}

/// `get-entries-model.ts` `formatFullEntry`.
fn format_full_navigation_entry(
    row: &NavigationRow,
    query: &ListQuery,
    private: &PrivatePermissions,
    access: &EntryAccess,
) -> Value {
    let mut entry = format_navigation_entry(row);

    entry.insert("isLocked".to_string(), Value::Bool(access.is_locked));
    entry.insert(
        "workbookTitle".to_string(),
        json_opt_string(&row.workbook_title),
    );
    entry.insert(
        "collectionTitle".to_string(),
        json_opt_string(&row.collection_title),
    );
    entry.insert("isFavorite".to_string(), Value::Bool(row.is_favorite));

    if let Some(unversioned_data) =
        private.filter_unversioned_data(&row.scope, row.unversioned_data.clone())
    {
        entry.insert("unversionedData".to_string(), unversioned_data);
    }

    if query.include_data {
        entry.insert("data".to_string(), row.data.clone().unwrap_or(Value::Null));
    }

    if query.include_links {
        entry.insert(
            "links".to_string(),
            row.links.clone().unwrap_or(Value::Null),
        );
    }

    if query.include_permissions_info {
        if let Some(permissions) = &access.permissions {
            entry.insert("permissions".to_string(), permissions.clone());
        }
    }

    Value::Object(entry)
}

/// `Utils.escapeStringForLike`.
fn escape_string_for_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '%' | '_' => {
                escaped.push('\\');
                escaped.push(character);
            }
            character => escaped.push(character),
        }
    }

    escaped
}

async fn run_list(
    state: &AppState,
    query: &ListQuery,
    cursor_mode: bool,
    login: &str,
    private: &PrivatePermissions,
    editor: bool,
    request_workbook_id: Option<i64>,
    is_private_route: bool,
) -> Result<Value, ApiError> {
    let tenant = state.config.tenant_id_override.as_str();
    let mut binds: Vec<Bind> = Vec::new();

    let cursor_fields: Vec<CursorField> = if !cursor_mode {
        Vec::new()
    } else {
        match query.order_by {
            None => vec![CursorField::EntryId],
            Some(order) => vec![
                match order.field {
                    OrderField::CreatedAt => CursorField::CreatedAt,
                    OrderField::Name => CursorField::SortName,
                },
                CursorField::EntryId,
            ],
        }
    };

    let mut sql = String::from("SELECT ");
    sql.push_str(NAVIGATION_COLUMNS);

    if cursor_mode {
        for (index, field) in cursor_fields.iter().enumerate() {
            sql.push_str(&format!(",\n    {}::text AS cursor{index}", field.sql()));
        }
        if cursor_fields.len() < 2 {
            sql.push_str(",\n    NULL::text AS cursor1");
        }
    } else {
        sql.push_str(",\n    NULL::text AS cursor0, NULL::text AS cursor1");
    }
    sql.push_str(",\n    NULL::bigint AS rev_id");

    let login_placeholder = Bind::placeholder(&mut binds, Bind::Str(login.to_string()));
    let tenant_placeholder = Bind::placeholder(&mut binds, Bind::Str(tenant.to_string()));

    sql.push_str(&format!(
        "\nFROM entries AS e\n\
         JOIN revisions AS r ON r.rev_id = e.saved_id\n\
         LEFT JOIN workbooks AS w ON w.workbook_id = e.workbook_id\n\
         LEFT JOIN collections AS c ON c.collection_id = e.collection_id\n\
         LEFT JOIN (\n\
         \x20   SELECT f.entity_id\n\
         \x20   FROM favorites AS f\n\
         \x20   WHERE f.login = {login_placeholder}\n\
         \x20     AND f.tenant_id = {tenant_placeholder}\n\
         \x20     AND f.entity_type = 'entry'\n\
         ) AS sub ON sub.entity_id = e.entry_id\n\
         WHERE e.is_deleted = FALSE\n\
         \x20 AND e.tenant_id = {tenant_placeholder}"
    ));

    if let Some(scope) = &query.scope {
        let placeholder = Bind::placeholder(&mut binds, Bind::Str(scope.clone()));
        sql.push_str(&format!("\n  AND e.scope::text = {placeholder}"));
    }

    // `x-dl-workbookid` scopes the editor: only entries of that workbook are
    // visible through a workbook-scoped request.
    if let Some(workbook_id) = request_workbook_id {
        let placeholder = Bind::placeholder(&mut binds, Bind::I64(workbook_id));
        sql.push_str(&format!("\n  AND e.workbook_id = {placeholder}"));
    }

    if let Some(ids) = &query.ids {
        if ids.is_empty() {
            // knex renders an empty `whereIn` as an always-false condition.
            sql.push_str("\n  AND 1 = 0");
        } else {
            let placeholders = ids
                .iter()
                .map(|id| Bind::placeholder(&mut binds, Bind::I64(*id)))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!("\n  AND e.entry_id IN ({placeholders})"));
        }
    }

    if let Some(types) = &query.types {
        if !types.is_empty() {
            let placeholders = types
                .iter()
                .map(|value| Bind::placeholder(&mut binds, Bind::Str(value.clone())))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!("\n  AND e.type IN ({placeholders})"));
        }
    }

    if let Some(created_by) = &query.created_by {
        if created_by.is_empty() {
            sql.push_str("\n  AND 1 = 0");
        } else {
            let placeholders = created_by
                .iter()
                .map(|value| Bind::placeholder(&mut binds, Bind::Str(value.clone())))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!("\n  AND e.created_by IN ({placeholders})"));
        }
    }

    if query.ignore_workbook_entries {
        sql.push_str("\n  AND e.workbook_id IS NULL");
    }

    if query.ignore_shared_entries {
        sql.push_str("\n  AND e.collection_id IS NULL");
    }

    if let Some(meta) = &query.meta {
        // `whereRaw('meta->>?::text = ?::text', …)` — `meta` is ambiguous across
        // the joined tables, so PostgreSQL rejects the statement, exactly like
        // the Node service (500 "Database error").
        for (field, value) in meta {
            let field_placeholder = Bind::placeholder(&mut binds, Bind::Str(field.clone()));
            let value_placeholder = Bind::placeholder(&mut binds, Bind::Str(value.clone()));
            sql.push_str(&format!(
                "\n  AND meta->>{field_placeholder}::text = {value_placeholder}::text"
            ));
        }
    }

    if let Some(name) = &query.filters_name {
        let pattern = format!("%{}%", escape_string_for_like(&name.to_lowercase()));
        let placeholder = Bind::placeholder(&mut binds, Bind::Str(pattern));
        sql.push_str(&format!("\n  AND e.name LIKE {placeholder}"));
    }

    if cursor_mode {
        if let Some(token) = &query.page_token {
            let values = decode_cursor_token(&cursor_fields, token)?;

            if cursor_fields.len() == 1 {
                let placeholder = Bind::placeholder(&mut binds, Bind::Str(values[0].clone()));
                sql.push_str(&format!("\n  AND e.entry_id > {placeholder}::text::bigint"));
            } else {
                let sort_field = cursor_fields[0];
                let ascending = query.order_by.map(|order| order.ascending).unwrap_or(true);
                let operator = if ascending { ">" } else { "<" };

                let first = Bind::placeholder(&mut binds, Bind::Str(values[0].clone()));
                let second = Bind::placeholder(&mut binds, Bind::Str(values[1].clone()));

                sql.push_str(&format!(
                    "\n  AND ({expr} {operator} {first}::text::{cast} OR ({expr} = {first}::text::{cast} AND e.entry_id > {second}::text::bigint))",
                    expr = sort_field.sql(),
                    cast = sort_field.cast(),
                ));
            }
        }
    }

    sql.push_str("\nORDER BY ");
    if let Some(order) = query.order_by {
        let direction = if order.ascending { "ASC" } else { "DESC" };
        let expression = match order.field {
            OrderField::CreatedAt => "e.created_at",
            OrderField::Name => "e.sort_name",
        };
        sql.push_str(&format!("{expression} {direction}, "));
    }
    sql.push_str("e.entry_id ASC");

    if cursor_mode {
        sql.push_str(&format!("\nLIMIT {}", query.page_size + 1));
    } else {
        sql.push_str(&format!(
            "\nLIMIT {} OFFSET {}",
            query.page_size,
            query.page_size.saturating_mul(query.page)
        ));
    }

    let mut request = sqlx::query_as::<_, NavigationRow>(&sql);
    for bind in binds {
        request = match bind {
            Bind::I64(value) => request.bind(value),
            Bind::Str(value) => request.bind(value),
        };
    }

    let mut rows = request.fetch_all(&state.db.replica).await?;

    let mut next_page_token: Option<String> = None;

    if cursor_mode {
        if rows.len() as i64 > query.page_size {
            let last = &rows[query.page_size as usize - 1];
            let mut values = vec![last.cursor0.clone().unwrap_or_default()];

            if cursor_fields.len() > 1 {
                values.push(last.cursor1.clone().unwrap_or_default());
            }

            next_page_token = Some(encode_cursor_token(&values));
            rows.truncate(query.page_size as usize);
        }
    } else if query.page >= 0 && rows.len() as i64 == query.page_size {
        // `Utils.getOptimisticNextPageToken`
        next_page_token = Some((query.page + 1).to_string());
    }

    // `filterEntriesByPermission` + `filterEntriesByIsLocked`
    // (`db/models/navigation/index.js`): every entry gets an access verdict,
    // locked parent entries are dropped, then `excludeLocked` decides whether
    // the remaining locked entries are filtered or returned as stubs.
    let accesses: Vec<EntryAccess> = if is_private_route {
        // `isPrivateRoute` short-circuits `checkEntriesByPermission` to
        // `makeEntryWithFullPermissions`.
        rows.iter()
            .map(|_| EntryAccess {
                is_locked: false,
                permissions: query
                    .include_permissions_info
                    .then(permissions::full_entry_permissions),
            })
            .collect()
    } else {
        let mut workbook_ids: Vec<i64> = rows.iter().filter_map(|row| row.workbook_id).collect();
        workbook_ids.sort_unstable();
        workbook_ids.dedup();
        let existing_workbooks = permissions::existing_workbook_ids(
            &state.db.replica,
            state.config.tenant_id_override.as_str(),
            &workbook_ids,
        )
        .await?;

        rows.iter()
            .map(|row| {
                let workbook_exists = row
                    .workbook_id
                    .is_some_and(|workbook_id| existing_workbooks.contains(&workbook_id));
                permissions::entry_access(
                    row.workbook_id,
                    row.collection_id,
                    workbook_exists,
                    editor,
                    query.include_permissions_info,
                )
            })
            .collect()
    };

    let entries: Vec<Value> = rows
        .iter()
        .zip(accesses.iter())
        .map(|(row, access)| format_full_navigation_entry(row, query, private, access))
        .collect();

    let entries = permissions::filter_entries_by_permission(entries);

    let entries: Vec<Value> = if query.exclude_locked {
        permissions::filter_entries_by_is_locked(entries)
    } else {
        entries
            .into_iter()
            .map(|entry| {
                if permissions::is_locked(&entry) {
                    permissions::locked_stub(&entry)
                } else {
                    entry
                }
            })
            .collect()
    };

    let mut body = Map::new();
    body.insert("entries".to_string(), Value::Array(entries));
    if let Some(token) = next_page_token {
        body.insert("nextPageToken".to_string(), Value::String(token));
    }

    Ok(Value::Object(body))
}

async fn list_entries(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, ApiError> {
    let is_private_route = uri.path().starts_with("/private");
    permissions::require_ctx_tenant_id(&state, is_private_route)?;
    let query = parse_list_query(uri.query())?;
    let login = resolve_login(&user);
    let private = PrivatePermissions::from_headers(&headers);
    let editor = is_private_route
        || permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let request_workbook_id = if is_private_route {
        None
    } else {
        permissions::request_workbook_id(&headers)
    };

    let body = run_list(
        &state,
        &query,
        false,
        &login,
        &private,
        editor,
        request_workbook_id,
        is_private_route,
    )
    .await?;

    Ok(Json(body).into_response())
}

async fn get_entries_v2(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let is_private_route = uri.path().starts_with("/private");
    permissions::require_ctx_tenant_id(&state, is_private_route)?;
    let query = parse_list_body(&body)?;
    let login = resolve_login(&user);
    let private = PrivatePermissions::from_headers(&headers);
    let editor = is_private_route
        || permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let request_workbook_id = if is_private_route {
        None
    } else {
        permissions::request_workbook_id(&headers)
    };

    let result = run_list(
        &state,
        &query,
        true,
        &login,
        &private,
        editor,
        request_workbook_id,
        is_private_route,
    )
    .await?;

    Ok(Json(result).into_response())
}

// ---------------------------------------------------------------------------
// POST /v1/check-entries-existence
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct EntryIdRow {
    entry_id: i64,
}

/// `zc.encodedIdArraySafe(…)` — decodes what it can and reports the rest.
fn decode_encoded_ids_safe(items: &[Value]) -> (Vec<i64>, Vec<String>) {
    let mut decoded = Vec::new();
    let mut failed = Vec::new();

    for item in items {
        let text = match item {
            Value::String(text) => text,
            _ => continue,
        };

        match decode_id(text) {
            Ok(id) => decoded.push(id),
            Err(_) => failed.push(text.clone()),
        }
    }

    (decoded, failed)
}

async fn check_entries_existence(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let Value::Object(map) = &body else {
        return Err(validation(vec![issue_invalid_type(
            "object",
            value_type_name(&body),
            json!([]),
        )]));
    };

    let mut issues = Vec::new();

    let (decoded, failed) = match map.get("entryIds") {
        None => {
            issues.push(issue_invalid_type(
                "array",
                "undefined",
                json!(["entryIds"]),
            ));
            (Vec::new(), Vec::new())
        }
        Some(Value::Array(items)) => {
            if items.is_empty() {
                issues.push(issue_too_small_array(json!(["entryIds"]), 1));
            } else if items.len() > 1000 {
                issues.push(issue_too_big_array(json!(["entryIds"]), 1000));
            }
            decode_encoded_ids_safe(items)
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "array",
                value_type_name(other),
                json!(["entryIds"]),
            ));
            (Vec::new(), Vec::new())
        }
    };

    if !issues.is_empty() {
        return Err(validation(issues));
    }

    let existing: Vec<i64> = if decoded.is_empty() {
        Vec::new()
    } else {
        let mut binds: Vec<Bind> = Vec::new();
        let placeholders = decoded
            .iter()
            .map(|id| Bind::placeholder(&mut binds, Bind::I64(*id)))
            .collect::<Vec<_>>()
            .join(", ");
        let tenant_placeholder = Bind::placeholder(
            &mut binds,
            Bind::Str(state.config.tenant_id_override.clone()),
        );

        let sql = format!(
            "SELECT entry_id FROM entries WHERE entry_id IN ({placeholders}) AND tenant_id = {tenant_placeholder} AND is_deleted = FALSE"
        );

        let mut request = sqlx::query_as::<_, EntryIdRow>(&sql);
        for bind in binds {
            request = match bind {
                Bind::I64(value) => request.bind(value),
                Bind::Str(value) => request.bind(value),
            };
        }

        request
            .fetch_all(&state.db.replica)
            .await?
            .into_iter()
            .map(|row| row.entry_id)
            .collect()
    };

    let mut response = Map::new();

    for id in &decoded {
        response.insert(encode_id(*id), Value::Bool(existing.contains(id)));
    }

    for raw in &failed {
        response.insert(raw.clone(), Value::Bool(false));
    }

    Ok(Json(Value::Object(response)).into_response())
}

// ---------------------------------------------------------------------------
// POST /v1/get-entries-data | get-entries-meta | get-entries-annotation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinedKind {
    Data,
    Meta,
    Annotation,
}

#[derive(Debug, Default)]
struct JoinedQuery {
    entry_ids: Vec<i64>,
    scope: Option<String>,
    entry_type: Option<String>,
    fields: Option<Vec<String>>,
}

fn parse_encoded_id_array(
    value: &Value,
    path: &str,
    min: usize,
    max: usize,
    issues: &mut Vec<Value>,
) -> Option<Vec<i64>> {
    let Value::Array(items) = value else {
        issues.push(issue_invalid_type(
            "array",
            value_type_name(value),
            json!([path]),
        ));
        return None;
    };

    if items.len() < min {
        issues.push(issue_too_small_array(json!([path]), min));
        return None;
    }

    if items.len() > max {
        issues.push(issue_too_big_array(json!([path]), max));
        return None;
    }

    let mut decoded = Vec::with_capacity(items.len());

    for item in items {
        let Value::String(text) = item else {
            issues.push(issue_invalid_type(
                "string",
                value_type_name(item),
                json!([path]),
            ));
            return None;
        };

        match decode_id(text) {
            Ok(id) => decoded.push(id),
            Err(_) => {
                issues.push(issue_custom(
                    json!([path]),
                    &format!("id '{text}' has incorrect format"),
                ));
                return None;
            }
        }
    }

    Some(decoded)
}

fn parse_joined_body(body: &Value, kind: JoinedKind) -> Result<JoinedQuery, ApiError> {
    let Value::Object(map) = body else {
        return Err(validation(vec![issue_invalid_type(
            "object",
            value_type_name(body),
            json!([]),
        )]));
    };

    let mut issues = Vec::new();
    let mut result = JoinedQuery::default();

    match map.get("entryIds") {
        None => issues.push(issue_invalid_type(
            "array",
            "undefined",
            json!(["entryIds"]),
        )),
        Some(value) => {
            if let Some(ids) = parse_encoded_id_array(value, "entryIds", 1, 1000, &mut issues) {
                result.entry_ids = ids;
            }
        }
    }

    if let Some(value) = map.get("scope") {
        match value {
            Value::String(text) if ENTRY_SCOPES.contains(&text.as_str()) => {
                result.scope = Some(text.clone());
            }
            Value::String(_) => issues.push(issue_invalid_value(json!(["scope"]), &ENTRY_SCOPES)),
            other => issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["scope"]),
            )),
        }
    }

    if let Some(value) = map.get("type") {
        match value {
            Value::String(text) => result.entry_type = Some(text.clone()),
            other => issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["type"]),
            )),
        }
    }

    if kind != JoinedKind::Annotation {
        match map.get("fields") {
            None => issues.push(issue_invalid_type("array", "undefined", json!(["fields"]))),
            Some(Value::Array(items)) => {
                if items.is_empty() {
                    issues.push(issue_too_small_array(json!(["fields"]), 1));
                } else if items.len() > 100 {
                    issues.push(issue_too_big_array(json!(["fields"]), 100));
                } else {
                    let mut fields = Vec::new();

                    for (index, item) in items.iter().enumerate() {
                        let Value::String(text) = item else {
                            issues.push(issue_invalid_type(
                                "string",
                                value_type_name(item),
                                json!(["fields", index]),
                            ));
                            continue;
                        };

                        if kind == JoinedKind::Meta && (text.contains('.') || text.contains('[')) {
                            issues.push(issue_custom(
                                json!(["fields", index]),
                                "nested fields or arrays are not supported",
                            ));
                            continue;
                        }

                        fields.push(text.clone());
                    }

                    result.fields = Some(fields);
                }
            }
            Some(other) => issues.push(issue_invalid_type(
                "array",
                value_type_name(other),
                json!(["fields"]),
            )),
        }
    }

    if issues.is_empty() {
        Ok(result)
    } else {
        Err(validation(issues))
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct JoinedRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    data: Option<Value>,
    meta: Option<Value>,
    annotation: Option<Value>,
}

/// `getJoinedEntriesRevisionsByIds`.
async fn fetch_joined_rows(
    state: &AppState,
    query: &JoinedQuery,
) -> Result<Vec<JoinedRow>, ApiError> {
    if query.entry_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut binds: Vec<Bind> = Vec::new();

    let placeholders = query
        .entry_ids
        .iter()
        .map(|id| Bind::placeholder(&mut binds, Bind::I64(*id)))
        .collect::<Vec<_>>()
        .join(", ");

    let tenant_placeholder = Bind::placeholder(
        &mut binds,
        Bind::Str(state.config.tenant_id_override.clone()),
    );

    let mut sql = format!(
        "SELECT e.entry_id AS entry_id, e.scope::text AS scope, e.type AS entry_type, \
         r.data AS data, r.meta AS meta, r.annotation AS annotation \
         FROM entries AS e \
         JOIN revisions AS r ON r.rev_id = COALESCE(e.published_id, e.saved_id) \
         WHERE e.entry_id IN ({placeholders}) AND e.tenant_id = {tenant_placeholder} AND e.is_deleted = FALSE"
    );

    if let Some(scope) = &query.scope {
        let placeholder = Bind::placeholder(&mut binds, Bind::Str(scope.clone()));
        sql.push_str(&format!(" AND e.scope::text = {placeholder}"));
    }

    if let Some(entry_type) = &query.entry_type {
        let placeholder = Bind::placeholder(&mut binds, Bind::Str(entry_type.clone()));
        sql.push_str(&format!(" AND e.type = {placeholder}"));
    }

    let mut request = sqlx::query_as::<_, JoinedRow>(&sql);
    for bind in binds {
        request = match bind {
            Bind::I64(value) => request.bind(value),
            Bind::Str(value) => request.bind(value),
        };
    }

    Ok(request.fetch_all(&state.db.replica).await?)
}

/// `_.get(object, path)` for the field subset `get-entries-data` accepts.
fn lodash_get<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();

    for character in path.chars() {
        match character {
            '.' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '[' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            ']' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '"' | '\'' => {}
            character => current.push(character),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    let mut node = value;
    for token in tokens {
        node = match node {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }

    Some(node)
}

fn entries_error(access_denied: bool, entry_id: i64) -> Value {
    let code = if access_denied {
        "ACCESS_DENIED"
    } else {
        "NOT_FOUND"
    };

    let mut error = Map::new();
    error.insert("code".to_string(), Value::String(code.to_string()));

    let mut item = Map::new();
    item.insert("entryId".to_string(), Value::String(encode_id(entry_id)));
    item.insert("error".to_string(), Value::Object(error));

    Value::Object(item)
}

fn format_joined_entries(kind: JoinedKind, rows: &[JoinedRow], query: &JoinedQuery) -> Vec<Value> {
    let fields = query.fields.clone().unwrap_or_default();

    query
        .entry_ids
        .iter()
        .map(|entry_id| {
            let row = rows.iter().find(|row| row.entry_id == *entry_id);

            let Some(row) = row else {
                return entries_error(false, *entry_id);
            };

            let mut result = Map::new();
            result.insert("scope".to_string(), Value::String(row.scope.clone()));
            result.insert("type".to_string(), Value::String(row.entry_type.clone()));

            match kind {
                JoinedKind::Data => {
                    let mut data = Map::new();

                    if let Some(value) = &row.data {
                        for field in &fields {
                            if let Some(found) = lodash_get(value, field) {
                                data.insert(field.clone(), found.clone());
                            }
                        }
                    }

                    result.insert("data".to_string(), Value::Object(data));
                }
                JoinedKind::Meta => {
                    let mut meta = Map::new();

                    if let Some(Value::Object(source)) = &row.meta {
                        for field in &fields {
                            if let Some(found) = source.get(field) {
                                meta.insert(field.clone(), found.clone());
                            }
                        }
                    }

                    result.insert("meta".to_string(), Value::Object(meta));
                }
                JoinedKind::Annotation => {
                    let annotation = match &row.annotation {
                        Some(Value::Null) | None => json!({}),
                        Some(value) => value.clone(),
                    };
                    result.insert("annotation".to_string(), annotation);
                }
            }

            let mut item = Map::new();
            item.insert(
                "entryId".to_string(),
                Value::String(encode_id(row.entry_id)),
            );
            item.insert("result".to_string(), Value::Object(result));

            Value::Object(item)
        })
        .collect()
}

async fn joined_entries_response(
    state: &AppState,
    body: &Value,
    kind: JoinedKind,
) -> Result<Response, ApiError> {
    let query = parse_joined_body(body, kind)?;
    let rows = fetch_joined_rows(state, &query).await?;
    let response = format_joined_entries(kind, &rows, &query);

    Ok(Json(Value::Array(response)).into_response())
}

async fn get_entries_data(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    joined_entries_response(&state, &body, JoinedKind::Data).await
}

async fn get_entries_meta(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    joined_entries_response(&state, &body, JoinedKind::Meta).await
}

async fn get_entries_annotation(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    joined_entries_response(&state, &body, JoinedKind::Annotation).await
}

// ---------------------------------------------------------------------------
// POST /v1/get-entries-relations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkDirection {
    From,
    To,
}

#[derive(Debug, Clone)]
struct EntriesRelationsQuery {
    entry_ids: Vec<i64>,
    link_direction: LinkDirection,
    include_permissions_info: bool,
    limit: i64,
    page_token: Option<String>,
    scope: Option<String>,
}

fn parse_entries_relations_body(body: &Value) -> Result<EntriesRelationsQuery, ApiError> {
    let Value::Object(map) = body else {
        return Err(validation(vec![issue_invalid_type(
            "object",
            value_type_name(body),
            json!([]),
        )]));
    };

    let mut issues = Vec::new();
    let mut entry_ids = Vec::new();

    match map.get("entryIds") {
        None => issues.push(issue_invalid_type(
            "array",
            "undefined",
            json!(["entryIds"]),
        )),
        Some(value) => {
            if let Some(ids) = parse_encoded_id_array(value, "entryIds", 1, 1000, &mut issues) {
                entry_ids = ids;
            }
        }
    }

    let link_direction = match map.get("linkDirection") {
        None => LinkDirection::From,
        Some(Value::String(value)) if value == "from" => LinkDirection::From,
        Some(Value::String(value)) if value == "to" => LinkDirection::To,
        Some(Value::String(_)) => {
            issues.push(issue_invalid_value(
                json!(["linkDirection"]),
                &LINK_DIRECTIONS,
            ));
            LinkDirection::From
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["linkDirection"]),
            ));
            LinkDirection::From
        }
    };

    let include_permissions_info = body_bool(map, "includePermissionsInfo", &mut issues);

    let limit = body_number(map, "limit", 1, Some(1000), Some(100), &mut issues).unwrap_or(100);

    let page_token = match map.get("pageToken") {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["pageToken"]),
            ));
            None
        }
    };

    let scope = match map.get("scope") {
        None => None,
        Some(Value::String(value)) if ENTRY_SCOPES.contains(&value.as_str()) => Some(value.clone()),
        Some(Value::String(_)) => {
            issues.push(issue_invalid_value(json!(["scope"]), &ENTRY_SCOPES));
            None
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                value_type_name(other),
                json!(["scope"]),
            ));
            None
        }
    };

    if !issues.is_empty() {
        return Err(validation(issues));
    }

    Ok(EntriesRelationsQuery {
        entry_ids,
        link_direction,
        include_permissions_info,
        limit,
        page_token,
        scope,
    })
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct EntriesRelationRow {
    entry_id: i64,
    key: Option<String>,
    scope: String,
    entry_type: String,
    created_at: DateTime<Utc>,
    is_public: bool,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    cursor0: Option<String>,
    cursor1: Option<String>,
}

async fn get_entries_relations(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let is_private_route = uri.path().starts_with("/private");
    permissions::require_ctx_tenant_id(&state, is_private_route)?;
    let query = parse_entries_relations_body(&body)?;
    let editor = is_private_route
        || permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));

    // `Array.from(new Set(rawEntryIds))`
    let mut entry_ids: Vec<i64> = Vec::new();
    for id in &query.entry_ids {
        if !entry_ids.contains(id) {
            entry_ids.push(*id);
        }
    }

    // existence check (no tenant filter, like the service)
    let mut binds: Vec<Bind> = Vec::new();
    let placeholders = entry_ids
        .iter()
        .map(|id| Bind::placeholder(&mut binds, Bind::I64(*id)))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT entry_id FROM entries WHERE entry_id IN ({placeholders}) AND is_deleted = FALSE"
    );

    let mut request = sqlx::query_as::<_, EntryIdRow>(&sql);
    for bind in binds {
        request = match bind {
            Bind::I64(value) => request.bind(value),
            Bind::Str(value) => request.bind(value),
        };
    }

    let found: Vec<i64> = request
        .fetch_all(&state.db.replica)
        .await?
        .into_iter()
        .map(|row| row.entry_id)
        .collect();

    if found.len() != entry_ids.len() {
        // Node reports the raw (numeric) ids here, not the encoded form.
        let missing: Vec<String> = entry_ids
            .iter()
            .filter(|id| !found.contains(id))
            .map(|id| id.to_string())
            .collect();

        return Err(ApiError::not_exist_entry().with_details(json!({"missingEntryIds": missing})));
    }

    let mut binds: Vec<Bind> = Vec::new();

    let tenant_placeholder = Bind::placeholder(
        &mut binds,
        Bind::Str(state.config.tenant_id_override.clone()),
    );

    let id_placeholders = entry_ids
        .iter()
        .map(|id| Bind::placeholder(&mut binds, Bind::I64(*id)))
        .collect::<Vec<_>>()
        .join(", ");

    let (join, link_column) = match query.link_direction {
        LinkDirection::From => ("links.to_id = e.entry_id", "links.from_id"),
        LinkDirection::To => ("links.from_id = e.entry_id", "links.to_id"),
    };

    let scope_sql = match &query.scope {
        Some(scope) => {
            let placeholder = Bind::placeholder(&mut binds, Bind::Str(scope.clone()));
            format!("AND e.scope::text = {placeholder}")
        }
        None => String::new(),
    };

    let cursor_sql = match &query.page_token {
        Some(token) => {
            let values =
                decode_cursor_token(&[CursorField::CreatedAt, CursorField::EntryId], token)?;

            let first = Bind::placeholder(&mut binds, Bind::Str(values[0].clone()));
            let second = Bind::placeholder(&mut binds, Bind::Str(values[1].clone()));

            format!(
                "AND (e.created_at > {first}::text::timestamptz OR (e.created_at = {first}::text::timestamptz AND e.entry_id > {second}::text::bigint))"
            )
        }
        None => String::new(),
    };

    let sql = format!(
        "SELECT e.entry_id AS entry_id, e.display_key AS key, e.scope::text AS scope, \
         e.type AS entry_type, e.created_at AS created_at, e.public AS is_public, \
         e.tenant_id AS tenant_id, e.workbook_id AS workbook_id, \
         e.collection_id AS collection_id, \
         e.created_at::text AS cursor0, e.entry_id::text AS cursor1 \
         FROM entries AS e \
         INNER JOIN links ON {join} \
         WHERE e.tenant_id = {tenant_placeholder} \
           AND {link_column} IN ({id_placeholders}) \
           AND e.is_deleted = FALSE \
           {scope_sql} {cursor_sql} \
         ORDER BY e.created_at ASC, e.entry_id ASC \
         LIMIT {}",
        query.limit + 1
    );

    let mut request = sqlx::query_as::<_, EntriesRelationRow>(&sql);
    for bind in binds {
        request = match bind {
            Bind::I64(value) => request.bind(value),
            Bind::Str(value) => request.bind(value),
        };
    }

    let mut rows = request.fetch_all(&state.db.replica).await?;

    let mut next_page_token: Option<String> = None;
    if rows.len() as i64 > query.limit {
        let last = &rows[query.limit as usize - 1];
        next_page_token = Some(encode_cursor_token(&[
            last.cursor0.clone().unwrap_or_default(),
            last.cursor1.clone().unwrap_or_default(),
        ]));
        rows.truncate(query.limit as usize);
    }

    // `checkWorkbookEntriesByPermission` / `checkCollectionEntriesByPermission`
    // / `checkFolderEntriesByPermission`: with the access service disabled
    // workbook and folder entries stay unlocked; collection entries make the
    // open-source registry constructor throw.
    let has_collection_entries = rows.iter().any(|row| row.collection_id.is_some());
    if has_collection_entries {
        return Err(ApiError::collection_entry_unsupported_scope());
    }

    let mut processed: Vec<&EntriesRelationRow> = rows
        .iter()
        .filter(|row| row.workbook_id.is_some())
        .collect();
    processed.extend(
        rows.iter()
            .filter(|row| row.workbook_id.is_none() && row.collection_id.is_none()),
    );

    let relations: Vec<Value> = processed
        .iter()
        .map(|row| {
            let mut item = Map::new();
            item.insert(
                "entryId".to_string(),
                Value::String(encode_id(row.entry_id)),
            );

            // `relation.workbookId || relation.collectionId ? null : relation.key`
            let key = if row.workbook_id.is_some() || row.collection_id.is_some() {
                Value::Null
            } else {
                json_opt_string(&row.key)
            };
            item.insert("key".to_string(), key);
            item.insert("scope".to_string(), Value::String(row.scope.clone()));
            item.insert("type".to_string(), Value::String(row.entry_type.clone()));
            item.insert(
                "createdAt".to_string(),
                Value::String(format_timestamp(row.created_at)),
            );
            item.insert("public".to_string(), Value::Bool(row.is_public));
            item.insert("tenantId".to_string(), json_opt_string(&row.tenant_id));
            item.insert("workbookId".to_string(), json_opt_id(row.workbook_id));
            item.insert("collectionId".to_string(), json_opt_id(row.collection_id));
            item.insert("isLocked".to_string(), Value::Bool(false));

            if query.include_permissions_info {
                item.insert(
                    "permissions".to_string(),
                    if row.workbook_id.is_some() {
                        permissions::entry_permissions(editor)
                    } else {
                        permissions::full_entry_permissions()
                    },
                );
            }

            Value::Object(item)
        })
        .collect();

    let mut response = Map::new();
    response.insert("relations".to_string(), Value::Array(relations));
    if let Some(token) = next_page_token {
        response.insert("nextPageToken".to_string(), Value::String(token));
    }

    Ok(Json(Value::Object(response)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamptz_pattern_matches_node_regex() {
        for valid in [
            "2026-11-02 22:32:12.558569+00",
            "2026-11-02T22:32:12+00",
            "2026-11-02 22:32:12.5+03:00",
            "2026-11-02 22:32:12-0300",
            "2023-09-11 08:47:20.751+00",
        ] {
            assert!(matches_timestamptz(valid), "{valid} should match");
        }

        for invalid in ["", "abc", "2026-11-02", "2026-11-02 22:32:12"] {
            assert!(!matches_timestamptz(invalid), "{invalid} should not match");
        }
    }

    #[test]
    fn cursor_token_codec_matches_node() {
        // Reference token produced by the Node service for
        // `{"scope":"widget","pageSize":3,"orderBy":{"field":"createdAt","direction":"desc"}}`.
        let token = "WyIyMDI2LTExLTAyIDIyOjMyOjEyLjU1ODU2OSswMCIsIjIzMzk2MjI0NzA3NzM5MDA1MjQiXQ";
        let values = decode_cursor_token(&[CursorField::CreatedAt, CursorField::EntryId], token)
            .expect("decode");

        assert_eq!(
            values,
            vec![
                "2026-11-02 22:32:12.558569+00".to_string(),
                "2339622470773900524".to_string()
            ]
        );
        assert_eq!(encode_cursor_token(&values), token);

        // `sort_name::text` values contain a literal backslash.
        let sort_token =
            "WyJcXHg2MzY4NjE3Mjc0NzMzYTIwNjI2MTcyMjA2MzY4NjE3Mjc0MjAwMDMxMzEzMiIsIjE1MDcxNjQ3ODAyMDMzNDc4NDMiXQ";
        let values =
            decode_cursor_token(&[CursorField::SortName, CursorField::EntryId], sort_token)
                .expect("decode");
        assert!(values[0].starts_with("\\x6368"));
        assert_eq!(encode_cursor_token(&values), sort_token);
    }

    #[test]
    fn cursor_token_errors() {
        let error = decode_cursor_token(&[CursorField::CreatedAt, CursorField::EntryId], "0")
            .expect_err("invalid token");
        assert_eq!(error.status.as_u16(), 400);
        assert_eq!(error.code, Some("INVALID_PAGE_TOKEN"));
        assert_eq!(
            error.message,
            "Invalid page token: Unexpected end of JSON input"
        );

        let error = encode_wrap("[]");
        assert_eq!(error.message, "Invalid page token");
    }

    fn encode_wrap(payload: &str) -> ApiError {
        let token = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        decode_cursor_token(&[CursorField::CreatedAt, CursorField::EntryId], &token)
            .expect_err("wrong length")
    }

    #[test]
    fn query_parser_matches_qs() {
        let query = RawQuery::parse(Some("scope=dash&page=1&pageSize=5"));
        assert_eq!(query.get("scope"), Some(&QVal::Str("dash".into())));
        assert_eq!(query.get("page"), Some(&QVal::Str("1".into())));

        let query = RawQuery::parse(Some("type=chart&type=table"));
        assert_eq!(
            query.get("type"),
            Some(&QVal::Arr(vec!["chart".into(), "table".into()]))
        );

        let query = RawQuery::parse(Some("meta[is_release]=true"));
        assert_eq!(
            query.get("meta"),
            Some(&QVal::Obj(vec![("is_release".into(), "true".into())]))
        );

        let query = RawQuery::parse(Some("ids=a,b"));
        assert_eq!(query.get("ids"), Some(&QVal::Str("a,b".into())));

        let query = RawQuery::parse(Some("ids%5B0%5D=a&ids%5B1%5D=b"));
        assert_eq!(
            query_array(query.get("ids").expect("ids")),
            vec!["a".to_string(), "b".to_string()]
        );

        let query = RawQuery::parse(Some("name=hello+world%21"));
        assert_eq!(query.get("name"), Some(&QVal::Str("hello world!".into())));
    }

    #[test]
    fn js_parse_int_matches_javascript() {
        assert_eq!(js_parse_int("5"), Some(5.0));
        assert_eq!(js_parse_int(" 5 "), Some(5.0));
        assert_eq!(js_parse_int("5abc"), Some(5.0));
        assert_eq!(js_parse_int("+5"), Some(5.0));
        assert_eq!(js_parse_int("-5"), Some(-5.0));
        assert_eq!(js_parse_int("5.9"), Some(5.0));
        assert_eq!(js_parse_int("0x10"), Some(0.0));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
    }

    #[test]
    fn string_boolean_matches_true_flags() {
        assert!(is_true_arg("true"));
        assert!(is_true_arg("TRUE"));
        assert!(is_true_arg("1"));
        assert!(!is_true_arg("false"));
        assert!(!is_true_arg("yes"));
        assert!(!is_true_arg(""));
    }

    #[test]
    fn bigint_validation() {
        assert!(is_valid_bigint("123"));
        assert!(is_valid_bigint("-123"));
        assert!(is_valid_bigint(""));
        assert!(!is_valid_bigint("12x"));
        assert!(!is_valid_bigint("1.5"));
    }
}
