//! `src/controllers/workbooks/*`, `src/services/new/workbook/*`,
//! `src/db/models/new/workbook*` and the shared plumbing also used by
//! [`crate::api::collections`].

use axum::extract::{OriginalUri, Path, State};
use axum::http::{StatusCode, Uri};
use axum::routing::{delete, get, post};
use axum::Extension;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool};

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v2/workbooks", get(list_workbooks).post(create_workbook)),
        ("/v2/workbooks-get-list-by-ids", post(list_by_ids)),
        (
            "/v2/workbooks/{workbook_id}",
            get(get_workbook).delete(delete_workbook),
        ),
        ("/v2/workbooks/{workbook_id}/update", post(update_workbook)),
        ("/v2/workbooks/{workbook_id}/move", post(move_workbook)),
        ("/v2/workbooks/{workbook_id}/copy", post(copy_workbook)),
        ("/v2/workbooks/{workbook_id}/entries", get(workbook_entries)),
        (
            "/private/v2/workbooks/{workbook_id}",
            get(private_get_workbook).delete(private_delete_workbook),
        ),
        (
            "/private/v2/workbooks/{workbook_id}/entries",
            get(private_workbook_entries),
        ),
        (
            "/private/v2/workbooks/{workbook_id}/update",
            post(private_update_workbook),
        ),
        ("/private/v2/workbooks", post(private_create_workbook)),
        (
            "/private/v2/workbooks/{workbook_id}/restore",
            post(restore_workbook),
        ),
        ("/private/all-workbooks", get(all_workbooks)),
        ("/v2/delete-workbooks", delete(delete_workbooks)),
        ("/v2/move-workbooks", post(move_workbooks)),
    ])
}

// ---------------------------------------------------------------------------
// Generic database plumbing
// ---------------------------------------------------------------------------

/// A bound SQL parameter. The Node `pg` driver sends every value untyped, so
/// the Rust port mirrors that with explicit casts in the SQL where necessary.
pub(crate) enum Bind {
    I64(i64),
    OptI64(Option<i64>),
    Str(String),
    OptStr(Option<String>),
    Bool(bool),
    Json(Value),
    I64List(Vec<i64>),
    StrList(Vec<String>),
}

pub(crate) async fn fetch_all<T>(
    pool: &PgPool,
    sql: &str,
    binds: &[Bind],
) -> Result<Vec<T>, ApiError>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut query = sqlx::query_as::<_, T>(sql);

    for bind in binds {
        query = match bind {
            Bind::I64(value) => query.bind(*value),
            Bind::OptI64(value) => query.bind(*value),
            Bind::Str(value) => query.bind(value.as_str()),
            Bind::OptStr(value) => query.bind(value.as_deref()),
            Bind::Bool(value) => query.bind(*value),
            Bind::Json(value) => query.bind(value.clone()),
            Bind::I64List(value) => query.bind(value.clone()),
            Bind::StrList(value) => query.bind(value.clone()),
        };
    }

    Ok(query.fetch_all(pool).await?)
}

pub(crate) async fn execute(pool: &PgPool, sql: &str, binds: &[Bind]) -> Result<u64, ApiError> {
    let mut query = sqlx::query(sql);

    for bind in binds {
        query = match bind {
            Bind::I64(value) => query.bind(*value),
            Bind::OptI64(value) => query.bind(*value),
            Bind::Str(value) => query.bind(value.as_str()),
            Bind::OptStr(value) => query.bind(value.as_deref()),
            Bind::Bool(value) => query.bind(*value),
            Bind::Json(value) => query.bind(value.clone()),
            Bind::I64List(value) => query.bind(value.clone()),
            Bind::StrList(value) => query.bind(value.clone()),
        };
    }

    Ok(query.execute(pool).await?.rows_affected())
}

// ---------------------------------------------------------------------------
// Query string parsing (express' extended query parser)
// ---------------------------------------------------------------------------

/// The bracketed/duplicated keys express hands to zod (`orderBy[field]`,
/// `filters[name]`, `scope=a&scope=b`) are kept verbatim; handlers address them
/// by their wire spelling.
pub(crate) struct QueryParams {
    pairs: Vec<(String, String)>,
}

impl QueryParams {
    pub(crate) fn parse(uri: &Uri) -> Self {
        let mut pairs = Vec::new();

        if let Some(query) = uri.query() {
            for part in query.split('&') {
                if part.is_empty() {
                    continue;
                }

                let (key, value) = match part.split_once('=') {
                    Some((key, value)) => (key, value),
                    None => (part, ""),
                };

                pairs.push((percent_decode(key), percent_decode(value)));
            }
        }

        Self { pairs }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.pairs.iter().any(|(candidate, _)| candidate == key)
    }

    /// `query[key]` of the wire contract. `None` when the bracket key is absent.
    pub(crate) fn nested(&self, key: &str, field: &str) -> Option<&str> {
        self.get(&format!("{key}[{field}]"))
    }

    pub(crate) fn has_nested(&self, key: &str) -> bool {
        let prefix = format!("{key}[");
        self.pairs
            .iter()
            .any(|(candidate, _)| candidate.starts_with(&prefix))
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() + 1 => {
                let high = bytes.get(index + 1).and_then(|b| hex_value(*b));
                let low = bytes.get(index + 2).and_then(|b| hex_value(*b));

                match (high, low) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
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

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Zod-compatible value coercion and issue shapes (zod v4, as used by us 1.196)
// ---------------------------------------------------------------------------

/// `zc.stringBoolean()`: `z.string().toLowerCase().transform(isTrueArg)`.
pub(crate) fn parse_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|raw| raw.to_lowercase()).as_deref(),
        Some("true") | Some("1")
    )
}

/// JavaScript `parseInt(value, 10)`; `None` is `NaN`.
pub(crate) fn js_parse_int(value: &str) -> Option<f64> {
    let trimmed = value.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    let mut sign = 1.0f64;

    if let Some(first) = bytes.first() {
        if *first == b'+' || *first == b'-' {
            if *first == b'-' {
                sign = -1.0;
            }
            index = 1;
        }
    }

    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }

    if index == start {
        return None;
    }

    let digits = &trimmed[start..index];
    let magnitude: f64 = digits.parse().ok()?;

    Some(sign * magnitude)
}

pub(crate) fn received_name(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

pub(crate) fn issue_invalid_type(path: &[&str], expected: &str, received: &str) -> Value {
    json!({
        "expected": expected,
        "code": "invalid_type",
        "path": path,
        "message": format!("Invalid input: expected {expected}, received {received}"),
    })
}

pub(crate) fn issue_custom(path: &[&str], message: impl Into<String>) -> Value {
    json!({
        "code": "custom",
        "path": path,
        "message": message.into(),
    })
}

pub(crate) fn issue_too_small_number(path: &[&str], minimum: i64) -> Value {
    json!({
        "origin": "number",
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": path,
        "message": format!("Too small: expected number to be >={minimum}"),
    })
}

pub(crate) fn issue_too_big_number(path: &[&str], maximum: i64) -> Value {
    json!({
        "origin": "number",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected number to be <={maximum}"),
    })
}

pub(crate) fn issue_too_big_string(path: &[&str], maximum: usize) -> Value {
    json!({
        "origin": "string",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected string to have <={maximum} characters"),
    })
}

pub(crate) fn issue_too_small_array(path: &[&str], minimum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_small",
        "minimum": minimum,
        "inclusive": true,
        "path": path,
        "message": format!("Too small: expected array to have >={minimum} items"),
    })
}

pub(crate) fn issue_too_big_array(path: &[&str], maximum: usize) -> Value {
    json!({
        "origin": "array",
        "code": "too_big",
        "maximum": maximum,
        "inclusive": true,
        "path": path,
        "message": format!("Too big: expected array to have <={maximum} items"),
    })
}

pub(crate) fn issue_invalid_enum(path: &[&str], values: &[&str]) -> Value {
    let quoted: Vec<String> = values.iter().map(|value| format!("\"{value}\"")).collect();

    json!({
        "code": "invalid_value",
        "values": values,
        "path": path,
        "message": format!("Invalid option: expected one of {}", quoted.join("|")),
    })
}

pub(crate) fn issue_invalid_union(
    path: &[&str],
    first_branch: Vec<Value>,
    second_branch: Vec<Value>,
) -> Value {
    json!({
        "code": "invalid_union",
        "errors": [first_branch, second_branch],
        "path": path,
        "message": "Invalid input",
    })
}

/// `zc.entityName()` — `!value.includes('/')` then `KEY_REG.test(value)`.
pub(crate) fn entity_name_issues(title: &str) -> Vec<Value> {
    let mut issues = Vec::new();

    if title.contains('/') {
        issues.push(issue_custom(
            &["title"],
            "String should not contain the symbol /",
        ));
    }

    if !key_reg_ok(title) {
        issues.push(issue_custom(
            &["title"],
            "String should start and end with A-Za-zА-Яа-яЁё0-9_@()% and can contain only \
             symbols: A-Za-zА-Яа-яЁё0-9_@()%.,:;'|-–—−$*& and spaces",
        ));
    }

    issues
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || matches!(c, 'А'..='Я' | 'а'..='я' | 'Ё' | 'ё')
}

fn is_key_start(c: char) -> bool {
    is_word_char(c) || matches!(c, '@' | '(' | ')' | '%')
}

fn is_key_middle(c: char) -> bool {
    is_key_start(c)
        || matches!(
            c,
            '.' | ',' | ':' | ';' | '\'' | '|' | ' ' | '-' | '–' | '—' | '−' | '$' | '*' | '&'
        )
        || matches!(
            c,
            '\u{00A0}' | '\u{180E}' | '\u{2000}'
                ..='\u{200B}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
        )
}

/// `/^[\wА-Яа-яЁё_@()%]([\wА-Яа-яЁё_@().,:;'\u00A0\u180E\u2000-\u200B\u202F\u205F\u3000\uFEFF| \-–—−$*&%]*[\wА-Яа-яЁё_@()%]+)?$/`
pub(crate) fn key_reg_ok(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();

    let Some(first) = chars.first() else {
        return false;
    };

    if !is_key_start(*first) {
        return false;
    }

    if chars.len() == 1 {
        return true;
    }

    let last = chars[chars.len() - 1];
    if !is_key_start(last) {
        return false;
    }

    chars[1..chars.len() - 1].iter().all(|c| is_key_middle(*c))
}

// ---------------------------------------------------------------------------
// Cursor pagination (`src/utils/cursor-pagination.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenValidator {
    /// `zc.stringSqlTimestampz()`
    Timestampz,
    /// `zc.stringBigInt()`
    BigInt,
    /// `z.string()`
    AnyString,
}

pub(crate) struct CursorField {
    /// SQL identifier used in the order-by and the cursor comparison.
    pub ident: &'static str,
    /// Type the untagged `pg` parameter is resolved to by Postgres.
    pub bind_cast: &'static str,
    pub asc: bool,
    pub validator: TokenValidator,
}

pub(crate) fn encode_page_token(values: &[String]) -> String {
    let payload = serde_json::to_string(values).expect("cursor values serialize");
    URL_SAFE_NO_PAD.encode(payload)
}

/// `Buffer.from(token, 'base64url')` is lenient: it ignores invalid trailing
/// bits instead of rejecting the whole token.
fn base64url_decode_lenient(input: &str) -> Vec<u8> {
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);

    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => continue,
        };

        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;

        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }

    out
}

fn decode_page_token_raw(token: &str) -> Result<Vec<Value>, ApiError> {
    let bytes = base64url_decode_lenient(token);
    let text = String::from_utf8_lossy(&bytes).into_owned();

    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(values)) => Ok(values),
        Ok(_) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("INVALID_PAGE_TOKEN"),
            "Invalid page token",
        )),
        Err(error) => {
            let message = if text.is_empty() {
                "Invalid page token: Unexpected end of JSON input".to_owned()
            } else {
                let chars: Vec<char> = text.chars().collect();
                let index = error
                    .column()
                    .saturating_sub(1)
                    .min(chars.len().saturating_sub(1));
                let token_char = chars.get(index).copied().unwrap_or(' ');
                format!(
                    "Invalid page token: Unexpected token '{token_char}', \"{text}\" is not valid JSON"
                )
            };

            Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                Some("INVALID_PAGE_TOKEN"),
                message,
            ))
        }
    }
}

pub(crate) fn decode_page_token(
    token: &str,
    fields: &[CursorField],
) -> Result<Vec<String>, ApiError> {
    let raw = decode_page_token_raw(token)?;

    if raw.len() != fields.len() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("INVALID_PAGE_TOKEN"),
            "Invalid page token",
        ));
    }

    let mut values = Vec::with_capacity(fields.len());

    for (value, field) in raw.iter().zip(fields.iter()) {
        let Value::String(text) = value else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                Some("INVALID_PAGE_TOKEN"),
                "Invalid page token",
            ));
        };

        let valid = match field.validator {
            TokenValidator::AnyString => true,
            TokenValidator::BigInt => is_bigint(text),
            TokenValidator::Timestampz => is_sql_timestampz(text),
        };

        if !valid {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                Some("INVALID_PAGE_TOKEN"),
                "Invalid page token: invalid cursor value",
            ));
        }

        values.push(text.clone());
    }

    Ok(values)
}

/// JS `BigInt(value)` acceptance (no `n` suffix, whitespace trimmed).
pub(crate) fn is_bigint(value: &str) -> bool {
    let trimmed = value.trim();

    if trimmed.is_empty() {
        return true;
    }

    let unsigned = trimmed
        .strip_prefix('+')
        .or_else(|| trimmed.strip_prefix('-'))
        .unwrap_or(trimmed);

    let digits = if let Some(hex) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        hex
    } else if let Some(octal) = unsigned
        .strip_prefix("0o")
        .or_else(|| unsigned.strip_prefix("0O"))
    {
        octal
    } else if let Some(binary) = unsigned
        .strip_prefix("0b")
        .or_else(|| unsigned.strip_prefix("0B"))
    {
        binary
    } else {
        return !unsigned.is_empty() && unsigned.chars().all(|c| c.is_ascii_digit());
    };

    !digits.is_empty()
}

/// `zc.stringSqlTimestampz()`: `^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:.\d{1,6})?[+-]\d{2}(?::?\d{2})?$`
pub(crate) fn is_sql_timestampz(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;

    fn take_digits(bytes: &[u8], index: &mut usize, count: usize) -> bool {
        for _ in 0..count {
            match bytes.get(*index) {
                Some(byte) if byte.is_ascii_digit() => *index += 1,
                _ => return false,
            }
        }
        true
    }

    fn tail(bytes: &[u8], start: usize) -> Option<usize> {
        // `[+-]\d{2}(?::?\d{2})?$`
        let mut index = start;
        match bytes.get(index) {
            Some(b'+') | Some(b'-') => index += 1,
            _ => return None,
        }
        if !take_digits(bytes, &mut index, 2) {
            return None;
        }
        if bytes.get(index) == Some(&b':') {
            index += 1;
            if take_digits(bytes, &mut index, 2) && index == bytes.len() {
                return Some(index);
            }
            return None;
        }
        if index < bytes.len() {
            if take_digits(bytes, &mut index, 2) && index == bytes.len() {
                return Some(index);
            }
            return None;
        }
        Some(index)
    }

    if !take_digits(bytes, &mut index, 4) || bytes.get(index) != Some(&b'-') {
        return false;
    }
    index += 1;
    if !take_digits(bytes, &mut index, 2) || bytes.get(index) != Some(&b'-') {
        return false;
    }
    index += 1;
    if !take_digits(bytes, &mut index, 2) {
        return false;
    }
    if !matches!(bytes.get(index), Some(b' ') | Some(b'T')) {
        return false;
    }
    index += 1;
    if !take_digits(bytes, &mut index, 2) || bytes.get(index) != Some(&b':') {
        return false;
    }
    index += 1;
    if !take_digits(bytes, &mut index, 2) || bytes.get(index) != Some(&b':') {
        return false;
    }
    index += 1;
    if !take_digits(bytes, &mut index, 2) {
        return false;
    }

    // `(?:.\d{1,6})?` — the dot is unescaped in the Node regex, so any single
    // character is accepted. Regex backtracking means the fraction may also be
    // skipped, so both alternatives are tried.
    let mut fraction_end = index;
    if index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() {
        fraction_end = index + 1;
        let mut digits = 0;
        while digits < 6
            && bytes
                .get(fraction_end)
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            fraction_end += 1;
            digits += 1;
        }
        if digits > 0 {
            if tail(bytes, fraction_end).is_some() {
                return true;
            }
        }
    }

    tail(bytes, index).is_some()
}

/// Builds the cursor `WHERE` fragment and returns it together with the values to
/// bind (in appearance order). `first_param` is the 1-based index of the first
/// placeholder to emit.
pub(crate) fn build_cursor_clause(
    fields: &[CursorField],
    values: &[String],
    first_param: usize,
) -> (String, Vec<String>) {
    let all_same_direction = fields.windows(2).all(|pair| pair[0].asc == pair[1].asc);

    let mut binds = Vec::new();

    if all_same_direction {
        let operator = if fields[0].asc { ">" } else { "<" };
        let mut placeholder = first_param;

        let idents: Vec<&str> = fields.iter().map(|field| field.ident).collect();
        let casts: Vec<String> = fields
            .iter()
            .map(|field| {
                let value = format!("${placeholder}::{}", field.bind_cast);
                placeholder += 1;
                value
            })
            .collect();

        for value in values {
            binds.push(value.clone());
        }

        return (
            format!("({}) {operator} ({})", idents.join(", "), casts.join(", ")),
            binds,
        );
    }

    let mut parts: Vec<String> = Vec::new();
    let mut previous: Vec<(&str, String)> = Vec::new();
    let mut placeholder = first_param;

    for (index, field) in fields.iter().enumerate() {
        let operator = if field.asc { ">" } else { "<" };
        let mut and_parts: Vec<String> = Vec::new();

        for (previous_ident, previous_value) in &previous {
            and_parts.push(format!(
                "{previous_ident} = ${placeholder}::{}",
                cast_for(fields, previous_ident)
            ));
            binds.push(previous_value.clone());
            placeholder += 1;
        }

        and_parts.push(format!(
            "{} {operator} ${placeholder}::{}",
            field.ident, field.bind_cast
        ));
        binds.push(values[index].clone());
        placeholder += 1;

        parts.push(format!("({})", and_parts.join(" AND ")));
        previous.push((field.ident, values[index].clone()));
    }

    (parts.join(" OR "), binds)
}

fn cast_for(fields: &[CursorField], ident: &str) -> &'static str {
    fields
        .iter()
        .find(|field| field.ident == ident)
        .map(|field| field.bind_cast)
        .unwrap_or("text")
}

// ---------------------------------------------------------------------------
// Shared response formatting
// ---------------------------------------------------------------------------

/// Node serializes `Date` as `toISOString()`: UTC, milliseconds, `Z` suffix.
pub(crate) fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `Utils.escapeStringForLike`.
pub(crate) fn escape_string_for_like(value: &str) -> String {
    value.replace('%', "\\%").replace('_', "\\_")
}

/// `Workbook.checkPermission(Update|Delete|...)` for the private-API bypass and
/// the access service disabled: every mutation needs an editor/admin role.
fn require_editor(state: &AppState, user: Option<&AuthUser>) -> Result<(), ApiError> {
    if permissions::is_editor_for(state, user) {
        Ok(())
    } else {
        Err(ApiError::access_service_permission_denied())
    }
}

/// `Workbook.getAllPermissions()` of the original registry plugin.
pub(crate) fn workbook_permissions(editor: bool) -> Value {
    permissions::workbook_permissions(editor)
}

/// `getEntryPermissionsByWorkbook` for this deployment.
pub(crate) fn workbook_entry_permissions(editor: bool) -> Value {
    permissions::entry_permissions(editor)
}

#[derive(Debug, Clone, FromRow)]
pub(crate) struct WorkbookRow {
    pub workbook_id: i64,
    pub collection_id: Option<i64>,
    pub title: String,
    pub description: Option<String>,
    pub tenant_id: String,
    pub meta: Value,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_by: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub is_template: Option<bool>,
    pub status: String,
    pub title_lower: String,
    pub deleted_at: Option<DateTime<Utc>>,
}

pub(crate) const WORKBOOK_COLUMNS: &str = r#"
    workbook_id,
    collection_id,
    title,
    description,
    tenant_id,
    meta,
    created_by,
    created_at,
    updated_by,
    updated_at,
    is_template,
    status::text AS status,
    title_lower,
    deleted_at
"#;

#[allow(clippy::too_many_arguments)]
pub(crate) fn workbook_json_parts(
    workbook_id: i64,
    collection_id: Option<i64>,
    title: &str,
    description: Option<&str>,
    tenant_id: &str,
    meta: &Value,
    created_by: &str,
    created_at: DateTime<Utc>,
    updated_by: Option<&str>,
    updated_at: DateTime<Utc>,
    status: &str,
    include_permissions: bool,
    editor: bool,
) -> Value {
    let mut map = Map::new();

    map.insert("workbookId".into(), json!(encode_id(workbook_id)));
    map.insert("collectionId".into(), json!(encode_id_opt(collection_id)));
    map.insert("title".into(), json!(title));
    map.insert("description".into(), json!(description));
    map.insert("tenantId".into(), json!(tenant_id));
    map.insert("meta".into(), meta.clone());
    map.insert("createdBy".into(), json!(created_by));
    map.insert("createdAt".into(), json!(format_timestamp(created_at)));
    map.insert("updatedBy".into(), json!(updated_by));
    map.insert("updatedAt".into(), json!(format_timestamp(updated_at)));
    map.insert("status".into(), json!(status));

    if include_permissions {
        map.insert("permissions".into(), workbook_permissions(editor));
    }

    Value::Object(map)
}

pub(crate) fn workbook_json(model: &WorkbookRow, include_permissions: bool, editor: bool) -> Value {
    workbook_json_parts(
        model.workbook_id,
        model.collection_id,
        &model.title,
        model.description.as_deref(),
        &model.tenant_id,
        &model.meta,
        &model.created_by,
        model.created_at,
        model.updated_by.as_deref(),
        model.updated_at,
        &model.status,
        include_permissions,
        editor,
    )
}

pub(crate) async fn fetch_workbook(
    pool: &PgPool,
    tenant: &str,
    workbook_id: i64,
) -> Result<Option<WorkbookRow>, ApiError> {
    fetch_workbook_impl(pool, Some(tenant), workbook_id).await
}

pub(crate) async fn fetch_workbook_impl(
    pool: &PgPool,
    tenant: Option<&str>,
    workbook_id: i64,
) -> Result<Option<WorkbookRow>, ApiError> {
    let sql = match tenant {
        Some(_) => format!(
            "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
             WHERE deleted_at IS NULL AND workbook_id = $1 AND tenant_id = $2 LIMIT 1"
        ),
        None => format!(
            "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
             WHERE deleted_at IS NULL AND workbook_id = $1 LIMIT 1"
        ),
    };

    let rows: Vec<WorkbookRow> = match tenant {
        Some(tenant) => {
            fetch_all(
                pool,
                &sql,
                &[Bind::I64(workbook_id), Bind::Str(tenant.to_owned())],
            )
            .await?
        }
        None => fetch_all(pool, &sql, &[Bind::I64(workbook_id)]).await?,
    };

    Ok(rows.into_iter().next())
}

pub(crate) async fn fetch_workbook_by_id_any_tenant(
    pool: &PgPool,
    workbook_id: i64,
) -> Result<Option<WorkbookRow>, ApiError> {
    let sql = format!(
        "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
         WHERE deleted_at IS NULL AND workbook_id = $1 LIMIT 1"
    );

    let rows: Vec<WorkbookRow> = fetch_all(pool, &sql, &[Bind::I64(workbook_id)]).await?;

    Ok(rows.into_iter().next())
}

/// `checkWorkbookByTitle`.
pub(crate) async fn workbook_title_exists(
    pool: &PgPool,
    tenant: &str,
    collection_id: Option<i64>,
    title_lower: &str,
) -> Result<bool, ApiError> {
    let sql = match collection_id {
        Some(_) => {
            "SELECT 1 AS one FROM workbooks WHERE tenant_id = $1 AND deleted_at IS NULL \
             AND collection_id = $2 AND title_lower = $3 LIMIT 1"
        }
        None => {
            "SELECT 1 AS one FROM workbooks WHERE tenant_id = $1 AND deleted_at IS NULL \
             AND collection_id IS NULL AND title_lower = $2 LIMIT 1"
        }
    };

    let rows: Vec<(i32,)> = match collection_id {
        Some(collection_id) => {
            fetch_all(
                pool,
                sql,
                &[
                    Bind::Str(tenant.to_owned()),
                    Bind::I64(collection_id),
                    Bind::Str(title_lower.to_owned()),
                ],
            )
            .await?
        }
        None => {
            fetch_all(
                pool,
                sql,
                &[
                    Bind::Str(tenant.to_owned()),
                    Bind::Str(title_lower.to_owned()),
                ],
            )
            .await?
        }
    };

    Ok(!rows.is_empty())
}

// ---------------------------------------------------------------------------
// GET /v2/workbooks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ListWorkbooksQuery {
    collection_id: Option<i64>,
    include_permissions_info: bool,
    filter_string: Option<String>,
    order_field: &'static str,
    order_direction: &'static str,
    only_my: bool,
    page: i64,
    page_size: i64,
}

fn validate_list_workbooks(query: &QueryParams) -> Result<ListWorkbooksQuery, ApiError> {
    let mut issues = Vec::new();
    let mut parsed = ListWorkbooksQuery {
        order_field: "title",
        order_direction: "ASC",
        page_size: 1000,
        ..Default::default()
    };

    match query.get("collectionId") {
        Some(raw) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(ApiError::encoded_id_issue("collectionId", raw)),
        },
        None => {}
    }

    parsed.include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    if let Some(raw) = query.get("filterString") {
        if raw.chars().count() > 200 {
            issues.push(issue_too_big_string(&["filterString"], 200));
        }
        parsed.filter_string = Some(raw.to_owned());
    }

    if let Some(raw) = query.get("orderField") {
        match raw {
            "title" => parsed.order_field = "title",
            "createdAt" => parsed.order_field = "createdAt",
            "updatedAt" => parsed.order_field = "updatedAt",
            _ => issues.push(issue_invalid_enum(
                &["orderField"],
                &["title", "createdAt", "updatedAt"],
            )),
        }
    }

    if let Some(raw) = query.get("orderDirection") {
        match raw {
            "asc" => parsed.order_direction = "ASC",
            "desc" => parsed.order_direction = "DESC",
            _ => issues.push(issue_invalid_enum(&["orderDirection"], &["asc", "desc"])),
        }
    }

    parsed.only_my = parse_flag(query.get("onlyMy"));

    if let Some(raw) = query.get("page") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["page"], "number", "undefined")),
            Some(value) if value < 0.0 => issues.push(issue_too_small_number(&["page"], 0)),
            Some(value) => parsed.page = value as i64,
        }
    }

    if let Some(raw) = query.get("pageSize") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["pageSize"], "number", "undefined")),
            Some(value) if value < 1.0 => issues.push(issue_too_small_number(&["pageSize"], 1)),
            Some(value) if value > 1000.0 => issues.push(issue_too_big_number(&["pageSize"], 1000)),
            Some(value) => parsed.page_size = value as i64,
        }
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

pub(crate) struct WorkbookListArgs {
    pub collection_id: Option<i64>,
    pub include_permissions_info: bool,
    pub filter_string: Option<String>,
    pub order_field: &'static str,
    pub order_direction: &'static str,
    pub only_my: bool,
    pub page: i64,
    pub page_size: i64,
}

pub(crate) async fn query_workbooks(
    state: &AppState,
    args: &WorkbookListArgs,
) -> Result<(Vec<WorkbookRow>, Option<String>), ApiError> {
    let tenant = state.config.tenant_id_override.clone();

    if let Some(collection_id) = args.collection_id {
        let parents =
            crate::api::collections::get_parent_ids(&state.db.replica, &tenant, &[collection_id])
                .await?;

        if parents.is_empty() {
            return Err(ApiError::collection_not_exists());
        }
    }

    let mut binds = vec![Bind::Str(tenant)];

    let mut sql = format!(
        "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
         WHERE deleted_at IS NULL AND tenant_id = $1"
    );

    if let Some(collection_id) = args.collection_id {
        binds.push(Bind::I64(collection_id));
        sql.push_str(&format!(" AND collection_id = ${}", binds.len()));
    } else {
        sql.push_str(" AND collection_id IS NULL");
    }

    if let Some(filter) = &args.filter_string {
        binds.push(Bind::Str(format!(
            "%{}%",
            escape_string_for_like(&filter.to_lowercase())
        )));
        sql.push_str(&format!(" AND title_lower LIKE ${}", binds.len()));
    }

    if args.only_my {
        binds.push(Bind::Str("systemId".to_owned()));
        sql.push_str(&format!(" AND created_by = ${}", binds.len()));
    }

    let order_column = match args.order_field {
        "title" => "sort_title",
        "createdAt" => "created_at",
        _ => "updated_at",
    };

    sql.push_str(&format!(
        " ORDER BY {order_column} {} LIMIT {} OFFSET {}",
        args.order_direction,
        args.page_size,
        args.page_size * args.page
    ));

    let workbooks: Vec<WorkbookRow> = fetch_all(&state.db.replica, &sql, &binds).await?;

    let next_page_token = if args.page >= 0 && workbooks.len() as i64 == args.page_size {
        Some((args.page + 1).to_string())
    } else {
        None
    };

    Ok((workbooks, next_page_token))
}

async fn list_workbooks(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let query = QueryParams::parse(&uri);
    let args = validate_list_workbooks(&query)?;

    let list_args = WorkbookListArgs {
        collection_id: args.collection_id,
        include_permissions_info: args.include_permissions_info,
        filter_string: args.filter_string,
        order_field: args.order_field,
        order_direction: args.order_direction,
        only_my: args.only_my,
        page: args.page,
        page_size: args.page_size,
    };

    let (workbooks, next_page_token) = query_workbooks(&state, &list_args).await?;

    let items: Vec<Value> = workbooks
        .iter()
        .map(|model| workbook_json(model, args.include_permissions_info, editor))
        .collect();

    let mut response = Map::new();
    response.insert("workbooks".into(), Value::Array(items));
    if let Some(token) = next_page_token {
        response.insert("nextPageToken".into(), json!(token));
    }

    Ok(Json(Value::Object(response)))
}

// ---------------------------------------------------------------------------
// POST /v2/workbooks-get-list-by-ids
// ---------------------------------------------------------------------------

pub(crate) fn validate_encoded_id_array(
    body: &Value,
    field: &str,
) -> Result<Option<Vec<i64>>, ApiError> {
    let mut issues = Vec::new();

    let value = body.get(field);

    let Some(value) = value else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[field], "array", "undefined"),
        ])));
    };

    let Value::Array(items) = value else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[field], "array", received_name(Some(value))),
        ])));
    };

    if items.len() < 1 {
        issues.push(issue_too_small_array(&[field], 1));
    }
    if items.len() > 1000 {
        issues.push(issue_too_big_array(&[field], 1000));
    }

    let mut ids = Vec::new();

    for item in items {
        match item {
            Value::String(raw) => match decode_id(raw) {
                Ok(id) => ids.push(id),
                Err(_) => issues.push(issue_custom(
                    &[field],
                    format!("id '{raw}' has incorrect format"),
                )),
            },
            other => issues.push(issue_invalid_type(
                &[field],
                "string",
                received_name(Some(other)),
            )),
        }
    }

    if issues.is_empty() {
        Ok(Some(ids))
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn list_by_ids(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let ids = validate_encoded_id_array(&body, "workbookIds")?.unwrap_or_default();

    let sql = format!(
        "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
         WHERE deleted_at IS NULL AND tenant_id = $1 AND workbook_id = ANY($2)"
    );

    let workbooks: Vec<WorkbookRow> = fetch_all(
        &state.db.replica,
        &sql,
        &[
            Bind::Str(state.config.tenant_id_override.clone()),
            Bind::I64List(ids),
        ],
    )
    .await?;

    Ok(Json(Value::Array(
        workbooks
            .iter()
            .map(|model| workbook_json(model, false, false))
            .collect(),
    )))
}

// ---------------------------------------------------------------------------
// GET /v2/workbooks/{workbookId}
// ---------------------------------------------------------------------------

async fn get_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    get_workbook_impl(&state, &raw_workbook_id, &uri, false, editor).await
}

async fn private_get_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, ApiError> {
    get_workbook_impl(&state, &raw_workbook_id, &uri, true, true).await
}

async fn get_workbook_impl(
    state: &AppState,
    raw_workbook_id: &str,
    uri: &Uri,
    is_private: bool,
    editor: bool,
) -> Result<Json<Value>, ApiError> {
    let workbook_id = decode_id(raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", raw_workbook_id))?;

    let query = QueryParams::parse(uri);
    let include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    let tenant = if is_private {
        None
    } else {
        Some(state.config.tenant_id_override.as_str())
    };

    let model = fetch_workbook_impl(&state.db.replica, tenant, workbook_id)
        .await?
        .ok_or_else(ApiError::workbook_not_exists)?;

    Ok(Json(workbook_json(
        &model,
        include_permissions_info,
        editor,
    )))
}

// ---------------------------------------------------------------------------
// GET /v2/workbooks/{workbookId}/entries
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WorkbookContentQuery {
    include_permissions_info: bool,
    created_by: Option<String>,
    scopes: Option<Vec<String>>,
    filter_name: Option<String>,
    order_by: Option<(String, bool)>,
    page: i64,
    page_size: i64,
}

fn validate_workbook_content(query: &QueryParams) -> Result<WorkbookContentQuery, ApiError> {
    const SCOPES: &[&str] = &[
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

    let mut issues = Vec::new();
    let mut parsed = WorkbookContentQuery {
        include_permissions_info: false,
        created_by: None,
        scopes: None,
        filter_name: None,
        order_by: None,
        page: 0,
        page_size: 1000,
    };

    parsed.include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    if let Some(raw) = query.get("createdBy") {
        parsed.created_by = Some(raw.to_owned());
    }

    // `z.enum(EntryScope).or(z.array(z.enum(EntryScope)))` — an invalid value is
    // reported as `invalid_union` with both branch errors.
    let scope_values = query.all("scope");
    if !scope_values.is_empty() {
        if scope_values.len() == 1 {
            let raw = scope_values[0];
            if SCOPES.contains(&raw) {
                parsed.scopes = Some(vec![raw.to_owned()]);
            } else {
                issues.push(issue_invalid_union(
                    &["scope"],
                    vec![issue_invalid_enum(&[], SCOPES)],
                    vec![issue_invalid_type(&[], "array", "string")],
                ));
            }
        } else {
            let invalid: Vec<usize> = scope_values
                .iter()
                .enumerate()
                .filter(|(_, raw)| !SCOPES.contains(raw))
                .map(|(index, _)| index)
                .collect();

            if invalid.is_empty() {
                parsed.scopes = Some(scope_values.iter().map(|raw| (*raw).to_owned()).collect());
            } else {
                let paths: Vec<String> = invalid.iter().map(|index| index.to_string()).collect();
                let second_branch: Vec<Value> = paths
                    .iter()
                    .map(|path| issue_invalid_enum(&[path.as_str()], SCOPES))
                    .collect();
                issues.push(issue_invalid_union(
                    &["scope"],
                    vec![issue_invalid_enum(&[], SCOPES)],
                    second_branch,
                ));
            }
        }
    }

    if let Some(raw) = query.nested("filters", "name") {
        parsed.filter_name = Some(raw.to_owned());
    }

    if query.has_nested("orderBy") {
        let field = query.nested("orderBy", "field");
        let direction = query.nested("orderBy", "direction");

        let valid_field = match field {
            None => {
                issues.push(issue_invalid_enum(
                    &["orderBy", "field"],
                    &["name", "createdAt"],
                ));
                None
            }
            Some("name") => Some("name"),
            Some("createdAt") => Some("createdAt"),
            Some(_) => {
                issues.push(issue_invalid_enum(
                    &["orderBy", "field"],
                    &["name", "createdAt"],
                ));
                None
            }
        };

        let valid_direction = match direction {
            None => {
                issues.push(issue_invalid_enum(
                    &["orderBy", "direction"],
                    &["asc", "desc"],
                ));
                None
            }
            Some("asc") => Some(false),
            Some("desc") => Some(true),
            Some(_) => {
                issues.push(issue_invalid_enum(
                    &["orderBy", "direction"],
                    &["asc", "desc"],
                ));
                None
            }
        };

        if let (Some(field), Some(desc)) = (valid_field, valid_direction) {
            parsed.order_by = Some((field.to_owned(), desc));
        }
    }

    if let Some(raw) = query.get("page") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["page"], "number", "undefined")),
            Some(value) if value < 0.0 => issues.push(issue_too_small_number(&["page"], 0)),
            Some(value) => parsed.page = value as i64,
        }
    }

    if let Some(raw) = query.get("pageSize") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["pageSize"], "number", "undefined")),
            Some(value) if value < 1.0 => issues.push(issue_too_small_number(&["pageSize"], 1)),
            Some(value) if value > 200.0 => issues.push(issue_too_big_number(&["pageSize"], 200)),
            Some(value) => parsed.page_size = value as i64,
        }
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

#[derive(Debug, FromRow)]
struct WorkbookEntryRow {
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
    rev_id: i64,
    meta: Option<Value>,
    hidden: Option<bool>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    tenant_id: Option<String>,
    mirrored: Option<bool>,
    is_favorite: bool,
}

async fn workbook_entries(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    workbook_entries_impl(&state, &raw_workbook_id, &uri, false, editor).await
}

async fn private_workbook_entries(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, ApiError> {
    workbook_entries_impl(&state, &raw_workbook_id, &uri, true, true).await
}

async fn workbook_entries_impl(
    state: &AppState,
    raw_workbook_id: &str,
    uri: &Uri,
    is_private: bool,
    editor: bool,
) -> Result<Json<Value>, ApiError> {
    let workbook_id = decode_id(raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", raw_workbook_id))?;

    let query = QueryParams::parse(uri);
    let args = validate_workbook_content(&query)?;

    let tenant = if is_private {
        None
    } else {
        Some(state.config.tenant_id_override.as_str())
    };

    if fetch_workbook_impl(&state.db.replica, tenant, workbook_id)
        .await?
        .is_none()
    {
        return Err(ApiError::workbook_not_exists());
    }

    let mut binds = vec![Bind::Str("unknown".to_owned())];
    let mut sql = String::from(
        "SELECT \
            e.scope::text AS scope, \
            e.type AS entry_type, \
            e.key, \
            e.inner_meta, \
            e.created_by, \
            e.created_at, \
            e.is_deleted, \
            e.deleted_at, \
            e.hidden, \
            e.display_key, \
            e.entry_id, \
            e.saved_id, \
            e.published_id, \
            e.tenant_id, \
            e.name, \
            e.sort_name, \
            e.public, \
            e.unversioned_data, \
            e.workbook_id, \
            e.collection_id, \
            e.mirrored, \
            r.data, \
            r.meta, \
            r.annotation, \
            r.updated_by, \
            r.updated_at, \
            r.rev_id, \
            r.links, \
            r.version, \
            r.source_version, \
            CASE WHEN f.entity_id IS NULL THEN FALSE ELSE TRUE END AS is_favorite \
         FROM entries AS e \
         JOIN revisions AS r ON r.rev_id = COALESCE(e.published_id, e.saved_id) \
         LEFT JOIN favorites AS f \
            ON f.entity_id = e.entry_id AND f.entity_type = 'entry' AND f.login = $1 \
         WHERE e.is_deleted = FALSE",
    );

    if !is_private {
        binds.push(Bind::Str(state.config.tenant_id_override.clone()));
        sql.push_str(&format!(" AND e.tenant_id = ${}", binds.len()));
    }

    binds.push(Bind::I64(workbook_id));
    sql.push_str(&format!(" AND e.workbook_id = ${}", binds.len()));

    if let Some(created_by) = &args.created_by {
        binds.push(Bind::Str(created_by.clone()));
        sql.push_str(&format!(" AND e.created_by = ${}", binds.len()));
    }

    if let Some(name) = &args.filter_name {
        binds.push(Bind::Str(format!(
            "%{}%",
            escape_string_for_like(&name.to_lowercase())
        )));
        sql.push_str(&format!(" AND e.name LIKE ${}", binds.len()));
    }

    if let Some(scopes) = &args.scopes {
        let mut placeholders = Vec::new();
        for scope in scopes {
            binds.push(Bind::Str(scope.clone()));
            placeholders.push(format!("${}::scope", binds.len()));
        }
        sql.push_str(&format!(" AND e.scope IN ({})", placeholders.join(", ")));
    }

    match &args.order_by {
        Some((field, desc)) => {
            let direction = if *desc { "DESC" } else { "ASC" };
            match field.as_str() {
                "createdAt" => {
                    sql.push_str(&format!(
                        " ORDER BY e.created_at {direction}, e.entry_id ASC"
                    ));
                }
                _ => {
                    sql.push_str(&format!(" ORDER BY e.sort_name {direction}"));
                }
            }
        }
        // `findPage` has no ORDER BY: the row order is whatever plan the
        // identical query produces on the reference database.
        None => {}
    }

    sql.push_str(&format!(
        " LIMIT {} OFFSET {}",
        args.page_size,
        args.page_size * args.page
    ));

    let entries: Vec<WorkbookEntryRow> = fetch_all(&state.db.replica, &sql, &binds).await?;

    let next_page_token = if args.page >= 0 && entries.len() as i64 == args.page_size {
        Some((args.page + 1).to_string())
    } else {
        None
    };

    let permissions = args
        .include_permissions_info
        .then(|| workbook_entry_permissions(editor));

    let items: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut map = Map::new();
            map.insert("entryId".into(), json!(encode_id(entry.entry_id)));
            map.insert("scope".into(), json!(entry.scope));
            map.insert("type".into(), json!(entry.entry_type));
            map.insert("key".into(), json!(entry.display_key));
            map.insert("displayKey".into(), json!(entry.display_key));
            map.insert("createdBy".into(), json!(entry.created_by));
            map.insert(
                "createdAt".into(),
                json!(format_timestamp(entry.created_at)),
            );
            map.insert("updatedBy".into(), json!(entry.updated_by));
            map.insert(
                "updatedAt".into(),
                json!(format_timestamp(entry.updated_at)),
            );
            map.insert("savedId".into(), json!(encode_id_opt(entry.saved_id)));
            map.insert(
                "publishedId".into(),
                json!(encode_id_opt(entry.published_id)),
            );
            map.insert("revId".into(), json!(encode_id(entry.rev_id)));
            map.insert("meta".into(), entry.meta.clone().unwrap_or(Value::Null));
            map.insert(
                "hidden".into(),
                entry.hidden.map(Value::Bool).unwrap_or(Value::Null),
            );
            map.insert("workbookId".into(), json!(encode_id_opt(entry.workbook_id)));
            map.insert(
                "collectionId".into(),
                json!(encode_id_opt(entry.collection_id)),
            );
            map.insert("tenantId".into(), json!(entry.tenant_id));
            map.insert("isFavorite".into(), json!(entry.is_favorite));
            map.insert("isLocked".into(), json!(false));
            if let Some(permissions) = &permissions {
                map.insert("permissions".into(), permissions.clone());
            }
            map.insert(
                "mirrored".into(),
                entry.mirrored.map(Value::Bool).unwrap_or(Value::Null),
            );
            Value::Object(map)
        })
        .collect();

    let mut response = Map::new();
    response.insert("entries".into(), Value::Array(items));
    if let Some(token) = next_page_token {
        response.insert("nextPageToken".into(), json!(token));
    }

    Ok(Json(Value::Object(response)))
}

// ---------------------------------------------------------------------------
// GET /private/all-workbooks
// ---------------------------------------------------------------------------

async fn all_workbooks(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, ApiError> {
    let query = QueryParams::parse(&uri);
    let mut issues = Vec::new();

    let page_size = match query.get("pageSize") {
        None => 1000i64,
        Some(raw) => match js_parse_int(raw) {
            None => {
                issues.push(issue_invalid_type(&["pageSize"], "number", "undefined"));
                1000
            }
            Some(value) if value < 1.0 => {
                issues.push(issue_too_small_number(&["pageSize"], 1));
                1000
            }
            Some(value) if value > 1000.0 => {
                issues.push(issue_too_big_number(&["pageSize"], 1000));
                1000
            }
            Some(value) => value as i64,
        },
    };

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let page_token = query.get("page").filter(|token| !token.is_empty());

    let cursor_fields = [
        CursorField {
            ident: "workbooks.created_at",
            bind_cast: "timestamptz",
            asc: true,
            validator: TokenValidator::Timestampz,
        },
        CursorField {
            ident: "workbooks.workbook_id",
            bind_cast: "bigint",
            asc: true,
            validator: TokenValidator::BigInt,
        },
    ];

    let mut binds = Vec::new();
    let mut sql = format!(
        "SELECT {WORKBOOK_COLUMNS}, \
         (workbooks.created_at)::text AS cursor0, \
         (workbooks.workbook_id)::text AS cursor1 \
         FROM workbooks WHERE deleted_at IS NULL"
    );

    if let Some(token) = page_token {
        let values = decode_page_token(token, &cursor_fields)?;
        let (clause, cursor_binds) = build_cursor_clause(&cursor_fields, &values, 1);
        sql.push_str(&format!(" AND ({clause})"));
        binds.extend(cursor_binds.into_iter().map(Bind::Str));
    }

    sql.push_str(&format!(
        " ORDER BY workbooks.created_at ASC, workbooks.workbook_id ASC LIMIT {}",
        page_size + 1
    ));

    #[derive(FromRow)]
    struct CursorWorkbookRow {
        #[sqlx(flatten)]
        model: WorkbookRow,
        cursor0: String,
        cursor1: String,
    }

    let rows: Vec<CursorWorkbookRow> = fetch_all(&state.db.replica, &sql, &binds).await?;

    let has_more = rows.len() as i64 > page_size;
    let visible = if has_more {
        page_size as usize
    } else {
        rows.len()
    };

    let items: Vec<Value> = rows[..visible]
        .iter()
        .map(|row| workbook_json(&row.model, false, false))
        .collect();

    let mut response = Map::new();
    response.insert("workbooks".into(), Value::Array(items));

    if has_more {
        let last = &rows[visible - 1];
        response.insert(
            "nextPageToken".into(),
            json!(encode_page_token(&[
                last.cursor0.clone(),
                last.cursor1.clone()
            ])),
        );
    }

    Ok(Json(Value::Object(response)))
}

// ---------------------------------------------------------------------------
// POST /v2/workbooks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CreateWorkbookBody {
    collection_id: Option<i64>,
    title: String,
    description: Option<String>,
    meta: Option<Value>,
    status: Option<String>,
}

fn validate_create_workbook(body: &Value) -> Result<CreateWorkbookBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name(Some(body))),
        ])));
    };

    let mut parsed = CreateWorkbookBody::default();

    match object.get("collectionId") {
        None | Some(Value::Null) => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(ApiError::encoded_id_issue("collectionId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["collectionId"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("title") {
        Some(Value::String(title)) => {
            parsed.title = title.clone();
            issues.extend(entity_name_issues(title));
        }
        other => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name(other),
        )),
    }

    match object.get("description") {
        None => {}
        Some(Value::String(description)) => parsed.description = Some(description.clone()),
        Some(other) => issues.push(issue_invalid_type(
            &["description"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("meta") {
        None => {}
        Some(Value::Object(_)) => {
            let meta = object.get("meta").unwrap().clone();
            let size = serde_json::to_string(&meta)
                .map(|s| s.chars().count())
                .unwrap_or(0);
            if size > 3000 {
                issues.push(json!({
                    "code": "custom",
                    "path": ["meta"],
                    "params": {"code": "OBJECT_SIZE_LIMIT_EXCEEDED"},
                    "message": "Object can't contain more than 3000 characters",
                }));
            }
            parsed.meta = Some(meta);
        }
        Some(other) => issues.push(issue_invalid_type(
            &["meta"],
            "record",
            received_name(Some(other)),
        )),
    }

    match object.get("status") {
        None => {}
        Some(Value::String(status)) => {
            const STATUSES: &[&str] = &["active", "creating", "deleting", "deleted"];
            if STATUSES.contains(&status.as_str()) {
                parsed.status = Some(status.clone());
            } else {
                issues.push(issue_invalid_enum(&["status"], STATUSES));
            }
        }
        Some(other) => issues.push(issue_invalid_type(
            &["status"],
            "string",
            received_name(Some(other)),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn create_workbook(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    create_workbook_impl(&state, &body).await
}

async fn private_create_workbook(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    permissions::require_ctx_tenant_id(&state, true)?;
    create_workbook_impl(&state, &body).await
}

async fn create_workbook_impl(state: &AppState, body: &Value) -> Result<Json<Value>, ApiError> {
    let args = validate_create_workbook(body)?;
    let tenant = state.config.tenant_id_override.clone();
    let title = args.title.trim().to_owned();
    let description = args.description.map(|value| value.trim().to_owned());

    if let Some(collection_id) = args.collection_id {
        let parents =
            crate::api::collections::get_parent_ids(&state.db.primary, &tenant, &[collection_id])
                .await?;

        if parents.is_empty() {
            return Err(ApiError::collection_not_exists().with_message(format!(
                "Cannot find parent collection with id – {}",
                encode_id(collection_id)
            )));
        }
    }

    if let Some(collection_id) = args.collection_id {
        crate::api::collections::fetch_collection(&state.db.primary, &tenant, collection_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    }

    if workbook_title_exists(
        &state.db.primary,
        &tenant,
        args.collection_id,
        &title.to_lowercase(),
    )
    .await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            Some("WORKBOOK_ALREADY_EXISTS"),
            format!("Workbook with title \"{title}\" already exists in this scope"),
        ));
    }

    let mut columns = vec![
        "title",
        "title_lower",
        "description",
        "tenant_id",
        "collection_id",
        "created_by",
        "updated_by",
    ];
    let mut placeholders = Vec::new();
    let mut binds = vec![
        Bind::Str(title.clone()),
        Bind::Str(title.to_lowercase()),
        Bind::OptStr(description),
        Bind::Str(tenant.clone()),
        Bind::OptI64(args.collection_id),
        Bind::Str("systemId".to_owned()),
        Bind::Str("systemId".to_owned()),
    ];

    for index in 1..=binds.len() {
        placeholders.push(format!("${index}"));
    }

    if let Some(meta) = args.meta {
        columns.push("meta");
        binds.push(Bind::Json(meta));
        placeholders.push(format!("${}", binds.len()));
    }

    if let Some(status) = args.status {
        columns.push("status");
        binds.push(Bind::Str(status));
        placeholders.push(format!("${}::workbook_status_enum", binds.len()));
    }

    let sql = format!(
        "INSERT INTO workbooks ({}) VALUES ({}) RETURNING {WORKBOOK_COLUMNS}",
        columns.join(", "),
        placeholders.join(", ")
    );

    let rows: Vec<WorkbookRow> = fetch_all(&state.db.primary, &sql, &binds).await?;
    let model = rows.into_iter().next().ok_or_else(ApiError::internal)?;

    Ok(Json(workbook_json(&model, false, false)))
}

// ---------------------------------------------------------------------------
// POST /v2/workbooks/{workbookId}/update
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct UpdateWorkbookBody {
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    meta: Option<Value>,
}

fn validate_update_workbook(body: &Value) -> Result<UpdateWorkbookBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name(Some(body))),
        ])));
    };

    let mut parsed = UpdateWorkbookBody::default();

    match object.get("title") {
        None => {}
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = Some(title.clone());
        }
        Some(other) => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("description") {
        None => {}
        Some(Value::String(description)) => parsed.description = Some(description.clone()),
        Some(other) => issues.push(issue_invalid_type(
            &["description"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("status") {
        None => {}
        Some(Value::String(status)) => {
            const STATUSES: &[&str] = &["active", "creating", "deleting", "deleted"];
            if STATUSES.contains(&status.as_str()) {
                parsed.status = Some(status.clone());
            } else {
                issues.push(issue_invalid_enum(&["status"], STATUSES));
            }
        }
        Some(other) => issues.push(issue_invalid_type(
            &["status"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("meta") {
        None => {}
        Some(Value::Object(_)) => {
            let meta = object.get("meta").unwrap().clone();
            let size = serde_json::to_string(&meta)
                .map(|s| s.chars().count())
                .unwrap_or(0);
            if size > 3000 {
                issues.push(json!({
                    "code": "custom",
                    "path": ["meta"],
                    "params": {"code": "OBJECT_SIZE_LIMIT_EXCEEDED"},
                    "message": "Object can't contain more than 3000 characters",
                }));
            }
            parsed.meta = Some(meta);
        }
        Some(other) => issues.push(issue_invalid_type(
            &["meta"],
            "record",
            received_name(Some(other)),
        )),
    }

    if issues.is_empty() {
        if parsed.title.is_none()
            && parsed.description.is_none()
            && parsed.status.is_none()
            && parsed.meta.is_none()
        {
            return Err(ApiError::validation(Value::Array(vec![json!({
                "code": "custom",
                "path": [],
                "message": "The request body must contain at least one of the following fields: \"title\", \"description\", \"status\", or \"meta\".",
            })])));
        }

        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn update_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    update_workbook_impl(&state, &raw_workbook_id, &body, false).await
}

async fn private_update_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    update_workbook_impl(&state, &raw_workbook_id, &body, true).await
}

async fn update_workbook_impl(
    state: &AppState,
    raw_workbook_id: &str,
    body: &Value,
    is_private: bool,
) -> Result<Json<Value>, ApiError> {
    let workbook_id = decode_id(raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", raw_workbook_id))?;
    let args = validate_update_workbook(body)?;

    let tenant = state.config.tenant_id_override.clone();
    let lookup_tenant = if is_private {
        None
    } else {
        Some(tenant.as_str())
    };
    let current = fetch_workbook_impl(&state.db.primary, lookup_tenant, workbook_id)
        .await?
        .ok_or_else(ApiError::workbook_not_exists)?;

    let new_title = args.title.map(|value| value.trim().to_owned());
    let new_description = args.description.map(|value| value.trim().to_owned());

    if let Some(title) = &new_title {
        if title.to_lowercase() != current.title_lower
            && workbook_title_exists(
                &state.db.primary,
                &tenant,
                current.collection_id,
                &title.to_lowercase(),
            )
            .await?
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                Some("WORKBOOK_ALREADY_EXISTS"),
                "The workbook already exists",
            ));
        }
    }

    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Bind> = vec![Bind::I64(workbook_id)];

    let mut push = |sql: &mut Vec<String>, binds: &mut Vec<Bind>, column: &str, value: Bind| {
        binds.push(value);
        sql.push(format!("{column} = ${}", binds.len()));
    };

    if let Some(title) = &new_title {
        push(&mut sets, &mut binds, "title", Bind::Str(title.clone()));
        push(
            &mut sets,
            &mut binds,
            "title_lower",
            Bind::Str(title.to_lowercase()),
        );
    }
    if let Some(description) = new_description {
        push(&mut sets, &mut binds, "description", Bind::Str(description));
    }
    if let Some(meta) = args.meta {
        push(&mut sets, &mut binds, "meta", Bind::Json(meta));
    }
    if let Some(status) = args.status {
        binds.push(Bind::Str(status));
        sets.push(format!("status = ${}::workbook_status_enum", binds.len()));
    }

    push(
        &mut sets,
        &mut binds,
        "updated_by",
        Bind::Str("systemId".to_owned()),
    );
    sets.push("updated_at = CURRENT_TIMESTAMP".to_owned());

    let sql = format!(
        "UPDATE workbooks SET {} WHERE workbook_id = $1 RETURNING {WORKBOOK_COLUMNS}",
        sets.join(", ")
    );

    let rows: Vec<WorkbookRow> = fetch_all(&state.db.primary, &sql, &binds).await?;
    let model = rows
        .into_iter()
        .next()
        .ok_or_else(ApiError::workbook_not_exists)?;

    Ok(Json(workbook_json(&model, false, false)))
}

// ---------------------------------------------------------------------------
// DELETE /v2/workbooks/{workbookId}, DELETE /v2/delete-workbooks
// ---------------------------------------------------------------------------

/// `markEntriesAsDeleted` — a bulk upsert that renames the entries into
/// `__trash` and flags them deleted.
pub(crate) async fn mark_entries_as_deleted(
    pool: &PgPool,
    entries: &[TrashEntry],
) -> Result<Vec<i64>, ApiError> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    let mut binds = Vec::new();

    for entry in entries {
        let name = entry
            .key
            .as_deref()
            .and_then(get_name_by_key)
            .unwrap_or_default();
        let new_key = format!("__trash/{}_{}", entry.entry_id, name);

        let mut inner_meta = match &entry.inner_meta {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        inner_meta.insert("oldKey".into(), json!(entry.key));
        inner_meta.insert("oldDisplayKey".into(), json!(entry.display_key));

        let params = [
            Bind::I64(entry.entry_id),
            Bind::Str(new_key.clone()),
            Bind::Str(new_key),
            Bind::Json(Value::Object(inner_meta)),
            Bind::Str(entry.updated_by.clone()),
            Bind::Bool(true),
            Bind::Str(entry.scope.clone()),
            Bind::Str(entry.entry_type.clone()),
            Bind::Str(entry.created_by.clone()),
        ];

        let mut placeholders = Vec::new();
        for bind in params {
            binds.push(bind);
            placeholders.push(format!("${}", binds.len()));
        }

        values.push(format!(
            "({}, {}, {}, {}, {}, {}, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, {}::scope, {}, {})",
            placeholders[0],
            placeholders[1],
            placeholders[2],
            placeholders[3],
            placeholders[4],
            placeholders[5],
            placeholders[6],
            placeholders[7],
            placeholders[8],
        ));
    }

    let sql = format!(
        "INSERT INTO entries \
         (entry_id, key, display_key, inner_meta, updated_by, is_deleted, updated_at, deleted_at, \
          scope, type, created_by) \
         VALUES {} \
         ON CONFLICT (entry_id) DO UPDATE SET \
            key = EXCLUDED.key, \
            display_key = EXCLUDED.display_key, \
            inner_meta = EXCLUDED.inner_meta, \
            updated_by = EXCLUDED.updated_by, \
            is_deleted = EXCLUDED.is_deleted, \
            updated_at = EXCLUDED.updated_at, \
            deleted_at = EXCLUDED.deleted_at, \
            scope = EXCLUDED.scope, \
            type = EXCLUDED.type, \
            created_by = EXCLUDED.created_by \
         RETURNING entry_id",
        values.join(", ")
    );

    let rows: Vec<(i64,)> = fetch_all(pool, &sql, &binds).await?;

    Ok(rows.into_iter().map(|row| row.0).collect())
}

pub(crate) struct TrashEntry {
    pub entry_id: i64,
    pub key: Option<String>,
    pub display_key: Option<String>,
    pub inner_meta: Option<Value>,
    pub updated_by: String,
    pub scope: String,
    pub entry_type: String,
    pub created_by: String,
}

fn get_name_by_key(key: &str) -> Option<String> {
    let without_trailing = key.strip_suffix('/').unwrap_or(key);
    without_trailing.rsplit('/').next().map(ToOwned::to_owned)
}

#[derive(Debug, FromRow)]
struct EntryForDeleteRow {
    entry_id: i64,
    key: Option<String>,
    display_key: Option<String>,
    inner_meta: Option<Value>,
    scope: String,
    entry_type: String,
    created_by: String,
}

pub(crate) async fn delete_workbooks_internal(
    state: &AppState,
    workbook_ids: &[i64],
) -> Result<Vec<WorkbookRow>, ApiError> {
    let tenant = state.config.tenant_id_override.clone();

    let sql = format!(
        "SELECT {WORKBOOK_COLUMNS} FROM workbooks \
         WHERE deleted_at IS NULL AND tenant_id = $1 AND workbook_id = ANY($2)"
    );
    let workbooks: Vec<WorkbookRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[Bind::Str(tenant), Bind::I64List(workbook_ids.to_vec())],
    )
    .await?;

    for workbook in &workbooks {
        if workbook.is_template.unwrap_or(false) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                Some("WORKBOOK_TEMPLATE_CANT_BE_DELETED"),
                "Workbook template can't be deleted",
            ));
        }
    }

    let ids: Vec<i64> = workbooks.iter().map(|model| model.workbook_id).collect();

    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let sql = format!(
        "UPDATE workbooks SET deleted_by = $1, deleted_at = CURRENT_TIMESTAMP \
         WHERE workbook_id = ANY($2) RETURNING {WORKBOOK_COLUMNS}"
    );
    let deleted: Vec<WorkbookRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[Bind::Str("systemId".to_owned()), Bind::I64List(ids.clone())],
    )
    .await?;

    let entries: Vec<EntryForDeleteRow> = fetch_all(
        &state.db.primary,
        "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, type AS entry_type, \
         created_by FROM entries WHERE is_deleted = FALSE AND workbook_id = ANY($1)",
        &[Bind::I64List(workbook_ids.to_vec())],
    )
    .await?;

    // `bulkCheckLock` — an entry with an unexpired lock aborts the delete.
    let entry_ids: Vec<i64> = entries.iter().map(|entry| entry.entry_id).collect();

    let locks: Vec<(i64, String, Option<DateTime<Utc>>)> = fetch_all(
        &state.db.primary,
        "SELECT entry_id, login, expiry_date FROM locks \
         WHERE entry_id = ANY($1) AND expiry_date > NOW()",
        &[Bind::I64List(entry_ids)],
    )
    .await?;

    if let Some((entry_id, login, expiry_date)) = locks.first() {
        return Err(ApiError::new(
            StatusCode::LOCKED,
            Some("ERR.US.ENTRY_IS_LOCKED"),
            "The entry is locked",
        )
        .with_details(json!({
            "entryId": encode_id(*entry_id),
            "loginOrId": login,
            "expiryDate": expiry_date.map(format_timestamp),
        })));
    }

    let trash: Vec<TrashEntry> = entries
        .into_iter()
        .map(|entry| TrashEntry {
            entry_id: entry.entry_id,
            key: entry.key,
            display_key: entry.display_key,
            inner_meta: entry.inner_meta,
            updated_by: "uid:systemId".to_owned(),
            scope: entry.scope,
            entry_type: entry.entry_type,
            created_by: entry.created_by,
        })
        .collect();

    mark_entries_as_deleted(&state.db.primary, &trash).await?;

    Ok(deleted)
}

async fn delete_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    delete_workbook_impl(&state, &raw_workbook_id).await
}

async fn private_delete_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    delete_workbook_impl(&state, &raw_workbook_id).await
}

async fn delete_workbook_impl(
    state: &AppState,
    raw_workbook_id: &str,
) -> Result<Json<Value>, ApiError> {
    let workbook_id = decode_id(raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", raw_workbook_id))?;

    let deleted = delete_workbooks_internal(state, &[workbook_id]).await?;
    let model = deleted.first().ok_or_else(ApiError::internal)?;

    Ok(Json(workbook_json(model, false, false)))
}

async fn delete_workbooks(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    let ids = validate_encoded_id_array(&body, "workbookIds")?.unwrap_or_default();

    let deleted = delete_workbooks_internal(&state, &ids).await?;
    let items: Vec<Value> = deleted
        .iter()
        .map(|model| workbook_json(model, false, false))
        .collect();

    Ok(Json(json!({"workbooks": items})))
}

// ---------------------------------------------------------------------------
// POST /private/v2/workbooks/{workbookId}/restore
// ---------------------------------------------------------------------------

async fn restore_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let workbook_id = decode_id(&raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", &raw_workbook_id))?;

    // `restoreWorkbook` is a private route: no tenant filter on the lookup.
    let sql = format!("SELECT {WORKBOOK_COLUMNS} FROM workbooks WHERE workbook_id = $1 LIMIT 1");
    let rows: Vec<WorkbookRow> =
        fetch_all(&state.db.primary, &sql, &[Bind::I64(workbook_id)]).await?;
    let model = rows
        .into_iter()
        .next()
        .ok_or_else(ApiError::workbook_not_exists)?;

    let Some(deleted_at) = model.deleted_at else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("WORKBOOK_IS_ALREADY_RESTORED"),
            "The workbook is already restored",
        ));
    };

    let sql = format!(
        "UPDATE workbooks SET deleted_by = NULL, deleted_at = NULL, \
         updated_at = CURRENT_TIMESTAMP \
         WHERE workbook_id = $1 AND tenant_id = $2 RETURNING {WORKBOOK_COLUMNS}"
    );

    let rows: Vec<WorkbookRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::I64(model.workbook_id),
            Bind::Str(model.tenant_id.clone()),
        ],
    )
    .await?;

    if rows.is_empty() {
        return Err(ApiError::workbook_not_exists());
    }

    execute(
        &state.db.primary,
        "UPDATE entries SET \
            key = regexp_replace(key, '__trash/', ''), \
            display_key = regexp_replace(display_key, '__trash/', ''), \
            inner_meta = inner_meta - 'oldKey' - 'oldDisplayKey', \
            is_deleted = FALSE, \
            deleted_at = NULL, \
            updated_at = CURRENT_TIMESTAMP \
         WHERE workbook_id = $1 AND tenant_id = $2 AND deleted_at >= $3::timestamptz",
        &[
            Bind::I64(model.workbook_id),
            Bind::Str(model.tenant_id.clone()),
            Bind::Str(format_timestamp(deleted_at)),
        ],
    )
    .await?;

    Ok(Json(json!({"workbookId": encode_id(model.workbook_id)})))
}

// ---------------------------------------------------------------------------
// POST /v2/workbooks/{workbookId}/move, POST /v2/move-workbooks
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MoveWorkbookBody {
    collection_id: Option<i64>,
    title: Option<String>,
}

fn validate_move_workbook(body: &Value) -> Result<MoveWorkbookBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name(Some(body))),
        ])));
    };

    let mut parsed = MoveWorkbookBody::default();

    match object.get("collectionId") {
        None => issues.push(issue_invalid_type(&["collectionId"], "string", "undefined")),
        Some(Value::Null) => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(ApiError::encoded_id_issue("collectionId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["collectionId"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("title") {
        None => {}
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = Some(title.clone());
        }
        Some(other) => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name(Some(other)),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

struct MoveWorkbookArgs {
    workbook_id: i64,
    collection_id: Option<i64>,
    title: Option<String>,
}

async fn move_workbook_impl(
    state: &AppState,
    args: MoveWorkbookArgs,
) -> Result<WorkbookRow, ApiError> {
    let tenant = state.config.tenant_id_override.clone();

    let workbook = fetch_workbook(&state.db.primary, &tenant, args.workbook_id)
        .await?
        .ok_or_else(ApiError::workbook_not_exists)?;

    if let Some(collection_id) = args.collection_id {
        crate::api::collections::fetch_collection(&state.db.primary, &tenant, collection_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    }

    let title = args
        .title
        .clone()
        .unwrap_or_else(|| workbook.title.clone())
        .trim()
        .to_owned();

    if workbook_title_exists(
        &state.db.primary,
        &tenant,
        args.collection_id,
        &title.to_lowercase(),
    )
    .await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            Some("WORKBOOK_ALREADY_EXISTS"),
            "The workbook already exists",
        ));
    }

    let sql = format!(
        "UPDATE workbooks SET title = $1, title_lower = $2, collection_id = $3, \
         updated_by = $4, updated_at = CURRENT_TIMESTAMP \
         WHERE workbook_id = $5 AND tenant_id = $6 RETURNING {WORKBOOK_COLUMNS}"
    );

    let rows: Vec<WorkbookRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str(title.clone()),
            Bind::Str(title.to_lowercase()),
            Bind::OptI64(args.collection_id),
            Bind::Str("systemId".to_owned()),
            Bind::I64(args.workbook_id),
            Bind::Str(tenant),
        ],
    )
    .await?;

    rows.into_iter()
        .next()
        .ok_or_else(ApiError::workbook_not_exists)
}

async fn move_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    let workbook_id = decode_id(&raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", &raw_workbook_id))?;
    let args = validate_move_workbook(&body)?;

    let model = move_workbook_impl(
        &state,
        MoveWorkbookArgs {
            workbook_id,
            collection_id: args.collection_id,
            title: args.title,
        },
    )
    .await?;

    Ok(Json(workbook_json(&model, false, false)))
}

async fn move_workbooks(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    let ids = validate_encoded_id_array(&body, "workbookIds")?.unwrap_or_default();

    let collection_id = match body.get("collectionId") {
        None => {
            return Err(ApiError::validation(Value::Array(vec![
                issue_invalid_type(&["collectionId"], "string", "undefined"),
            ])))
        }
        Some(Value::Null) => None,
        Some(Value::String(raw)) => Some(decode_id(raw).map_err(|_| {
            ApiError::validation_issues(vec![ApiError::encoded_id_issue("collectionId", raw)])
        })?),
        Some(other) => {
            return Err(ApiError::validation(Value::Array(vec![
                issue_invalid_type(&["collectionId"], "string", received_name(Some(other))),
            ])))
        }
    };

    let mut models = Vec::new();
    for workbook_id in ids {
        models.push(
            move_workbook_impl(
                &state,
                MoveWorkbookArgs {
                    workbook_id,
                    collection_id,
                    title: None,
                },
            )
            .await?,
        );
    }

    let items: Vec<Value> = models
        .iter()
        .map(|model| workbook_json(model, false, false))
        .collect();

    Ok(Json(json!({"workbooks": items})))
}

// ---------------------------------------------------------------------------
// POST /v2/workbooks/{workbookId}/copy
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CopyWorkbookBody {
    collection_id: Option<i64>,
    title: String,
}

fn validate_copy_workbook(body: &Value) -> Result<CopyWorkbookBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name(Some(body))),
        ])));
    };

    let mut parsed = CopyWorkbookBody::default();

    match object.get("collectionId") {
        None | Some(Value::Null) => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(ApiError::encoded_id_issue("collectionId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["collectionId"],
            "string",
            received_name(Some(other)),
        )),
    }

    match object.get("title") {
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = title.clone();
        }
        other => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name(other),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn copy_workbook(
    State(state): State<AppState>,
    Path(raw_workbook_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    let workbook_id = decode_id(&raw_workbook_id)
        .map_err(|_| ApiError::invalid_entry_id_for("workbookId", &raw_workbook_id))?;
    let args = validate_copy_workbook(&body)?;
    let tenant = state.config.tenant_id_override.clone();

    let origin = fetch_workbook_by_id_any_tenant(&state.db.replica, workbook_id)
        .await?
        .ok_or_else(ApiError::workbook_not_exists)?;

    if origin.tenant_id != tenant {
        return Err(ApiError::workbook_not_exists());
    }

    if let Some(collection_id) = args.collection_id {
        crate::api::collections::fetch_collection(&state.db.primary, &tenant, collection_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    }

    let origin_entries: Vec<CopyEntryRow> = fetch_all(
        &state.db.replica,
        "SELECT entry_id, scope::text AS scope, type AS entry_type, key, display_key, hidden, \
         published_id, unversioned_data, saved_id FROM entries \
         WHERE workbook_id = $1 AND tenant_id = $2 AND is_deleted = FALSE",
        &[Bind::I64(workbook_id), Bind::Str(origin.tenant_id.clone())],
    )
    .await?;

    let has_file_connection = origin_entries.iter().any(|entry| {
        entry.scope == "connection"
            && matches!(entry.entry_type.as_str(), "file" | "gsheets_v2" | "yadocs")
    });

    if has_file_connection {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("WORKBOOK_COPY_FILE_CONNECTION_ERROR"),
            "Copying workbooks with file connections is forbidden",
        ));
    }

    let title = args.title.trim().to_owned();
    let unique_title =
        unique_workbook_title(&state.db.primary, &tenant, args.collection_id, &title).await?;

    let sql = format!(
        "INSERT INTO workbooks \
         (title, title_lower, description, tenant_id, collection_id, meta, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {WORKBOOK_COLUMNS}"
    );

    let rows: Vec<WorkbookRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str(unique_title.clone()),
            Bind::Str(unique_title.to_lowercase()),
            Bind::OptStr(origin.description.clone()),
            Bind::Str(tenant.clone()),
            Bind::OptI64(args.collection_id),
            Bind::Json(origin.meta.clone()),
            Bind::Str("systemId".to_owned()),
            Bind::Str("systemId".to_owned()),
        ],
    )
    .await?;

    let new_workbook = rows.into_iter().next().ok_or_else(ApiError::internal)?;

    if !origin_entries.is_empty() {
        copy_entries(&state, &origin_entries, new_workbook.workbook_id, &tenant).await?;
    }

    Ok(Json(workbook_json(&new_workbook, false, false)))
}

#[derive(Debug, FromRow)]
struct CopyEntryRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    key: Option<String>,
    display_key: Option<String>,
    hidden: Option<bool>,
    published_id: Option<i64>,
    unversioned_data: Option<Value>,
    saved_id: Option<i64>,
}

#[derive(Debug, FromRow)]
struct CopyRevisionRow {
    data: Option<Value>,
    meta: Option<Value>,
    annotation: Option<Value>,
    links: Option<Value>,
}

async fn unique_workbook_title(
    pool: &PgPool,
    tenant: &str,
    collection_id: Option<i64>,
    title: &str,
) -> Result<String, ApiError> {
    let escaped = escape_string_for_like(&title.to_lowercase());
    let like_pattern = format!("{escaped} (copy %)");
    let not_like_pattern = format!("{escaped} (copy %)_%");

    let sql = "SELECT title_lower FROM workbooks \
         WHERE tenant_id = $1 AND deleted_at IS NULL \
           AND collection_id IS NOT DISTINCT FROM $2 \
           AND (title_lower = $3 OR (title_lower LIKE $4 AND title_lower NOT LIKE $5))";

    let rows: Vec<(String,)> = fetch_all(
        pool,
        sql,
        &[
            Bind::Str(tenant.to_owned()),
            Bind::OptI64(collection_id),
            Bind::Str(title.to_lowercase()),
            Bind::Str(like_pattern),
            Bind::Str(not_like_pattern),
        ],
    )
    .await?;

    if rows.is_empty() {
        return Ok(title.to_owned());
    }

    let mut max_counter = 1i64;

    for (title_lower,) in &rows {
        if title_lower == &title.to_lowercase() {
            continue;
        }

        if let Some(counter) = trailing_counter(title_lower) {
            max_counter = max_counter.max(counter + 1);
        }
    }

    Ok(format!("{title} (Copy {max_counter})"))
}

fn trailing_counter(value: &str) -> Option<i64> {
    let trimmed = value.strip_suffix(')')?;
    let mut digits = String::new();

    for c in trimmed.chars().rev() {
        if c.is_ascii_digit() {
            digits.insert(0, c);
        } else {
            break;
        }
    }

    if digits.is_empty() {
        return None;
    }

    digits.parse::<i64>().ok()
}

async fn copy_entries(
    state: &AppState,
    origin_entries: &[CopyEntryRow],
    destination_workbook_id: i64,
    target_tenant: &str,
) -> Result<(), ApiError> {
    let mut mapping: Vec<(String, String)> = Vec::new();
    let mut created: Vec<(i64, i64)> = Vec::new();

    for entry in origin_entries {
        let new_entry_id: i64 = sqlx::query_scalar("SELECT get_id()")
            .fetch_one(&state.db.primary)
            .await?;
        let new_rev_id: i64 = sqlx::query_scalar("SELECT get_id()")
            .fetch_one(&state.db.primary)
            .await?;

        let name = entry
            .display_key
            .as_deref()
            .and_then(get_name_by_key)
            .ok_or_else(ApiError::internal)?;

        let display_key = format!("{}/{name}", new_entry_id);
        let key = display_key.to_lowercase();

        let revision: Option<CopyRevisionRow> = fetch_all(
            &state.db.replica,
            "SELECT data, meta, annotation, links FROM revisions \
             WHERE rev_id = COALESCE($1, $2) LIMIT 1",
            &[
                Bind::OptI64(entry.published_id),
                Bind::OptI64(entry.saved_id),
            ],
        )
        .await?
        .into_iter()
        .next();

        let revision = revision.unwrap_or(CopyRevisionRow {
            data: None,
            meta: None,
            annotation: None,
            links: None,
        });

        fetch_all::<(i64,)>(
            &state.db.primary,
            "INSERT INTO entries \
             (entry_id, scope, type, key, inner_meta, created_by, updated_by, hidden, display_key, \
              saved_id, published_id, tenant_id, unversioned_data, workbook_id) \
             VALUES ($1, $2::scope, $3, $4, NULL, $5, $5, $6, $7, $8, $9, $10, $11, $12) \
             RETURNING entry_id",
            &[
                Bind::I64(new_entry_id),
                Bind::Str(entry.scope.clone()),
                Bind::Str(entry.entry_type.clone()),
                Bind::Str(key),
                Bind::Str("uid:systemId".to_owned()),
                Bind::Bool(entry.hidden.unwrap_or(false)),
                Bind::Str(display_key),
                Bind::I64(new_rev_id),
                Bind::OptI64(entry.published_id.map(|_| new_rev_id)),
                Bind::Str(target_tenant.to_owned()),
                Bind::Json(entry.unversioned_data.clone().unwrap_or(json!({}))),
                Bind::I64(destination_workbook_id),
            ],
        )
        .await?;

        fetch_all::<(i64,)>(
            &state.db.primary,
            "INSERT INTO revisions \
             (rev_id, entry_id, data, meta, annotation, created_by, updated_by, links) \
             VALUES ($1, $2, $3, $4, $5, $6, $6, $7) RETURNING rev_id",
            &[
                Bind::I64(new_rev_id),
                Bind::I64(new_entry_id),
                Bind::Json(revision.data.unwrap_or(Value::Null)),
                Bind::Json(revision.meta.unwrap_or(Value::Null)),
                Bind::Json(revision.annotation.unwrap_or(Value::Null)),
                Bind::Str("uid:systemId".to_owned()),
                Bind::Json(revision.links.unwrap_or(Value::Null)),
            ],
        )
        .await?;

        mapping.push((encode_id(entry.entry_id), encode_id(new_entry_id)));
        created.push((new_rev_id, new_entry_id));
    }

    // `crossSyncCopiedJoinedEntryRevisions`: replace the old encoded ids in the
    // copied revisions' JSON payloads.
    for (rev_id, entry_id) in created {
        let rows: Vec<(Option<Value>, Option<Value>, Option<Value>)> = fetch_all(
            &state.db.primary,
            "SELECT data, meta, links FROM revisions WHERE rev_id = $1",
            &[Bind::I64(rev_id)],
        )
        .await?;

        let Some((data, meta, links)) = rows.into_iter().next() else {
            continue;
        };

        let data = replace_ids(data, &mapping);
        let meta = replace_ids(meta, &mapping);
        let links = replace_ids(links, &mapping);

        // `Link.sync` for the copied entry (outgoing links only).
        execute(
            &state.db.primary,
            "DELETE FROM links WHERE from_id = $1",
            &[Bind::I64(entry_id)],
        )
        .await?;

        if let Some(Value::Object(links_map)) = &links {
            let mut target_ids = Vec::with_capacity(links_map.len());
            let mut target_names = Vec::with_capacity(links_map.len());

            for (name, target) in links_map {
                let Value::String(encoded) = target else {
                    continue;
                };

                let to_id = decode_id(encoded).map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        Some("INCORRECT_LINK_ERROR"),
                        "Some of the provided links are incorrect",
                    )
                })?;

                target_ids.push(to_id);
                target_names.push(name.clone());
            }

            if !target_ids.is_empty() {
                execute(
                    &state.db.primary,
                    "INSERT INTO links (from_id, to_id, name) \
                     SELECT $1, l.to_id, l.name FROM UNNEST($2::bigint[], $3::text[]) AS l(to_id, name) \
                     ON CONFLICT DO NOTHING",
                    &[
                        Bind::I64(entry_id),
                        Bind::I64List(target_ids),
                        Bind::StrList(target_names),
                    ],
                )
                .await?;
            }
        }

        execute(
            &state.db.primary,
            "UPDATE revisions SET data = $2, meta = $3, links = $4 WHERE rev_id = $1",
            &[
                Bind::I64(rev_id),
                Bind::Json(data.unwrap_or(Value::Null)),
                Bind::Json(meta.unwrap_or(Value::Null)),
                Bind::Json(links.unwrap_or(Value::Null)),
            ],
        )
        .await?;
    }

    Ok(())
}

fn replace_ids(value: Option<Value>, mapping: &[(String, String)]) -> Option<Value> {
    let value = value?;
    if mapping.is_empty() {
        return Some(value);
    }

    let mut text = value.to_string();
    for (old, new) in mapping {
        text = text.replace(old.as_str(), new.as_str());
    }

    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Small helpers used by `collections.rs`
// ---------------------------------------------------------------------------

impl ApiError {
    /// Validation issue for an arbitrary encoded id field. `ApiError` only
    /// exposes `invalid_entry_id`, which hardcodes the `entryId` path.
    pub(crate) fn invalid_entry_id_for(field: &'static str, value: &str) -> ApiError {
        ApiError::validation(json!([encoded_id_issue(field, value)]))
    }

    pub(crate) fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }
}

pub(crate) fn encoded_id_issue(field: &str, value: &str) -> Value {
    let length = value.chars().count();
    let expected = crate::id::ENCODED_ID_LENGTH;

    if length == expected {
        json!({
            "code": "custom",
            "message": format!("id '{value}' has incorrect format"),
            "path": [field],
        })
    } else if length < expected {
        json!({
            "origin": "string",
            "code": "too_small",
            "minimum": expected,
            "inclusive": true,
            "exact": true,
            "path": [field],
            "message": format!("Too small: expected string to have >={expected} characters"),
        })
    } else {
        json!({
            "origin": "string",
            "code": "too_big",
            "maximum": expected,
            "inclusive": true,
            "exact": true,
            "path": [field],
            "message": format!("Too big: expected string to have <={expected} characters"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_parse_int_matches_javascript() {
        assert_eq!(js_parse_int("12"), Some(12.0));
        assert_eq!(js_parse_int(" 12"), Some(12.0));
        assert_eq!(js_parse_int("12abc"), Some(12.0));
        assert_eq!(js_parse_int("1.5"), Some(1.0));
        assert_eq!(js_parse_int("-3"), Some(-3.0));
        assert_eq!(js_parse_int("+3"), Some(3.0));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("0x10"), Some(0.0));
    }

    #[test]
    fn flags_match_string_boolean() {
        for value in ["true", "TRUE", "1"] {
            assert!(parse_flag(Some(value)));
        }
        for value in ["false", "0", "yes", ""] {
            assert!(!parse_flag(Some(value)));
        }
        assert!(!parse_flag(None));
    }

    #[test]
    fn cursor_tokens_round_trip() {
        let fields = [
            CursorField {
                ident: "workbooks.created_at",
                bind_cast: "timestamptz",
                asc: true,
                validator: TokenValidator::Timestampz,
            },
            CursorField {
                ident: "workbooks.workbook_id",
                bind_cast: "bigint",
                asc: true,
                validator: TokenValidator::BigInt,
            },
        ];

        let values = vec![
            "2023-09-11 08:47:20.751846+00".to_string(),
            "1507164764046888724".to_string(),
        ];

        let token = encode_page_token(&values);
        assert_eq!(
            token,
            "WyIyMDIzLTA5LTExIDA4OjQ3OjIwLjc1MTg0NiswMCIsIjE1MDcxNjQ3NjQwNDY4ODg3MjQiXQ"
        );
        assert_eq!(decode_page_token(&token, &fields).unwrap(), values);
    }

    #[test]
    fn structure_item_token_round_trips() {
        let fields = [
            CursorField {
                ident: "u.rank",
                bind_cast: "integer",
                asc: true,
                validator: TokenValidator::AnyString,
            },
            CursorField {
                ident: "u.sort_title",
                bind_cast: "bytea",
                asc: true,
                validator: TokenValidator::AnyString,
            },
            CursorField {
                ident: "u.entity_id",
                bind_cast: "bigint",
                asc: true,
                validator: TokenValidator::BigInt,
            },
        ];

        let values = vec![
            "2".to_string(),
            "\\x4f70656e536f757263652044656d6f".to_string(),
            "1507164764046888724".to_string(),
        ];

        let token = encode_page_token(&values);
        assert!(token.starts_with("WyIyIiwiXFx4"));
        assert_eq!(decode_page_token(&token, &fields).unwrap(), values);
    }

    #[test]
    fn timestampz_validator_matches_node_regex() {
        assert!(is_sql_timestampz("2023-09-11 08:47:20.751846+00"));
        assert!(is_sql_timestampz("2023-09-11T08:47:20+03"));
        assert!(is_sql_timestampz("2023-09-11 08:47:20+0300"));
        assert!(!is_sql_timestampz("2023-09-11 08:47:20.751846"));
        assert!(!is_sql_timestampz("bogus"));
    }

    #[test]
    fn entity_name_validation_matches_key_reg() {
        assert!(key_reg_ok("OpenSource Demo"));
        assert!(key_reg_ok("МЧТ"));
        assert!(key_reg_ok("a"));
        assert!(key_reg_ok("a b-c_d@(x)%"));
        assert!(!key_reg_ok(""));
        assert!(!key_reg_ok(" leading"));
        assert!(!key_reg_ok("trailing "));
        assert!(!key_reg_ok("bad/title"));
        assert!(!key_reg_ok("a&"));
    }

    #[test]
    fn escape_like_matches_node() {
        assert_eq!(escape_string_for_like("a%b_c"), "a\\%b\\_c");
    }

    #[test]
    fn trailing_counter_extracts_copy_number() {
        assert_eq!(trailing_counter("zz test wb (copy 3)"), Some(3));
        assert_eq!(trailing_counter("zz test wb"), None);
    }
}
