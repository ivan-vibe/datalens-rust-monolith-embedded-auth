//! `src/controllers/states/*` — get-state, create-state.
//!
//! Ports of `src/services/new/state/*` and `src/components/hash-generator.ts`.
//!
//! ## The hash
//!
//! ```js
//! const str = entryId + JSON.stringify(sortObj(data));   // entryId is the DECODED id
//! let crc = CRC32.str(str);                              // npm `crc-32`
//! if (crc < 0) crc += 4294967296;                        // signed → unsigned
//! crc = crc.toString(16);
//! hash = ('00000000' + crc).slice(-8) + str.length;      // hex + decimal length
//! ```
//!
//! Two JavaScript quirks have to be reproduced for byte-identical hashes:
//!
//! * `sortObj` sorts keys with the default `Array#sort`, i.e. by **UTF-16 code
//!   units** — not by code point (Rust's `str` ordering) — and recurses into
//!   arrays without sorting them;
//! * `JSON.stringify` uses ECMAScript `Number::toString`: `1e21` serializes as
//!   `1e+21` (not Rust's `1e21`), `100` as `100`, and every number is first
//!   rounded to an IEEE-754 double, so `12345678901234567890` becomes
//!   `12345678901234567000`.
//!
//! `str.length` counts UTF-16 code units, which is why the length suffix is
//! computed per `char::len_utf16`.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Number, Value};

use crate::error::ApiError;
use crate::id::{decode_id, encode_id};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/states/{entry_id}/{hash}", get(get_state)),
        ("/v1/states/{entry_id}", post(create_state)),
    ])
}

/// `MAX_STATE_DATA_OBJECT_SYMBOLS` (`src/const/common.ts`)
const MAX_STATE_DATA_OBJECT_SYMBOLS: usize = 50_000;

// ---------------------------------------------------------------------------
// errors / responses
// ---------------------------------------------------------------------------

/// JSON presenter error or an express body-parser response (`Invalid JSON
/// supplied` is `text/html` and never reaches the error presenter).
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

/// `US_ERRORS.NOT_EXIST_STATE_BY_HASH` — note the doubled "has doesn't" of the
/// original presenter, reproduced verbatim.
fn not_exist_state_by_hash() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_STATE_BY_HASH"),
        "The state by this hash has doesn't exist",
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
// body parsing (express body-parser)
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// JavaScript-compatible JSON serialization
// ---------------------------------------------------------------------------

/// ECMAScript `Number::toString` for a finite `f64`.
fn js_number_string(value: f64) -> String {
    if value == 0.0 {
        // covers `-0` — `JSON.stringify(-0)` is `"0"`.
        return "0".to_owned();
    }

    if !value.is_finite() {
        return if value.is_nan() {
            "NaN".to_owned()
        } else if value > 0.0 {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        };
    }

    let negative = value < 0.0;
    let magnitude = value.abs();

    // Rust's `{:e}` prints the shortest round-tripping digit string, which is
    // exactly the (s, k) pair the ECMAScript algorithm is defined on.
    let formatted = format!("{magnitude:e}");
    let (mantissa, exponent) = formatted.split_once('e').expect("LowerExp always has 'e'");
    let exponent: i32 = exponent.parse().expect("integer exponent");

    let digits: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    let k = digits.len() as i32;
    let n = exponent + 1;

    let mut out = String::new();

    if negative {
        out.push('-');
    }

    if k <= n && n <= 21 {
        out.push_str(digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 >= 0 { '+' } else { '-' });
        out.push_str(&(n - 1).abs().to_string());
    }

    out
}

/// `JSON.stringify` string escaping.
fn js_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');

    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            ch if (ch as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }

    out.push('"');
    out
}

/// `Array#sort` default comparator: UTF-16 code units.
fn js_key_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn write_json(value: &Value, out: &mut String, sort_keys: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => {
            out.push_str(&js_number_string(number.as_f64().unwrap_or(f64::NAN)));
        }
        Value::String(text) => out.push_str(&js_string_literal(text)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out, sort_keys);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');

            if sort_keys {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_by(|left, right| js_key_cmp(left, right));

                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&js_string_literal(key));
                    out.push(':');
                    write_json(&map[key], out, sort_keys);
                }
            } else {
                for (index, (key, item)) in map.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&js_string_literal(key));
                    out.push(':');
                    write_json(item, out, sort_keys);
                }
            }

            out.push('}');
        }
    }
}

/// `JSON.stringify(value)` — the value as the JS engine would print it.
fn js_json(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out, false);
    out
}

/// `JSON.stringify(sortObj(value))`.
fn js_json_sorted(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out, true);
    out
}

/// `hashGenerator({entryId, data})` — `entryId` is the decoded decimal id.
fn hash_generator(entry_id: i64, data: &Value) -> String {
    let mut payload = entry_id.to_string();
    payload.push_str(&js_json_sorted(data));

    let crc = crc32fast::hash(payload.as_bytes());

    let mut hash = format!("{crc:08x}");
    hash.push_str(
        &payload
            .chars()
            .map(char::len_utf16)
            .sum::<usize>()
            .to_string(),
    );

    hash
}

// ---------------------------------------------------------------------------
// database helpers
// ---------------------------------------------------------------------------

/// Rows needed by `getEntry` (`src/services/new/entry/get-entry`): the entry,
/// its tenant, its workbook/collection and the *saved* revision (the default
/// `branch`).
const ENTRY_CHECK_SQL: &str = r#"
SELECT
    e.saved_id           AS saved_id,
    e.workbook_id        AS workbook_id,
    e.collection_id      AS collection_id,
    e.tenant_id          AS tenant_id,
    t.tenant_id          AS tenant_exists,
    t.deleting           AS tenant_deleting,
    w.tenant_id          AS workbook_tenant_id,
    w.deleted_at         AS workbook_deleted_at,
    c.tenant_id          AS collection_tenant_id,
    c.deleted_at         AS collection_deleted_at,
    r.rev_id             AS saved_rev_id
FROM entries AS e
LEFT JOIN tenants AS t ON t.tenant_id = e.tenant_id
LEFT JOIN workbooks AS w ON w.workbook_id = e.workbook_id
LEFT JOIN collections AS c ON c.collection_id = e.collection_id
LEFT JOIN revisions AS r ON r.rev_id = e.saved_id
WHERE e.entry_id = $1
  AND e.is_deleted = FALSE
  AND e.tenant_id = $2
LIMIT 1
"#;

#[derive(Debug, sqlx::FromRow)]
struct EntryCheckRow {
    saved_id: Option<i64>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    tenant_id: Option<String>,
    tenant_exists: Option<String>,
    tenant_deleting: Option<bool>,
    workbook_tenant_id: Option<String>,
    workbook_deleted_at: Option<DateTime<Utc>>,
    collection_tenant_id: Option<String>,
    collection_deleted_at: Option<DateTime<Utc>>,
    saved_rev_id: Option<i64>,
}

/// `getEntry({ctx}, {entryId})` — the checks that can fail in this build.
/// `accessServiceEnabled`/`dlsEnabled` are off, so permissions never throw;
/// collection entries do throw from the open-source `CollectionEntry`
/// constructor.
async fn ensure_entry(state: &AppState, entry_id: i64) -> Result<(), ApiError> {
    let row: Option<EntryCheckRow> = sqlx::query_as(ENTRY_CHECK_SQL)
        .bind(entry_id)
        .bind(state.config.tenant_id_override.as_str())
        .fetch_optional(&state.db.replica)
        .await?;

    let Some(row) = row else {
        return Err(ApiError::not_exist_entry());
    };

    // `const revision = entry?.publishedRevision ?? entry?.savedRevision ?? …`
    if row.saved_id.is_none() || row.saved_rev_id.is_none() {
        return Err(ApiError::not_exist_entry());
    }

    if row.tenant_exists.is_none() {
        return Err(ApiError::not_exist_tenant());
    }

    if row.tenant_deleting.unwrap_or(false) {
        return Err(ApiError::tenant_is_being_deleted());
    }

    if row.workbook_id.is_some() {
        if row.workbook_tenant_id.is_none() || row.workbook_deleted_at.is_some() {
            return Err(ApiError::workbook_not_exists());
        }

        if row.workbook_tenant_id.as_deref() != row.tenant_id.as_deref() {
            return Err(ApiError::entry_and_workbook_tenant_mismatch());
        }
    } else if row.collection_id.is_some() {
        if row.collection_tenant_id.is_none() || row.collection_deleted_at.is_some() {
            return Err(ApiError::collection_not_exists());
        }

        if row.collection_tenant_id.as_deref() != row.tenant_id.as_deref() {
            return Err(ApiError::entry_and_collection_tenant_mismatch());
        }

        // `checkCollectionEntry` builds the registry `CollectionEntry` entity,
        // whose open-source constructor throws unconditionally.
        return Err(ApiError::collection_entry_unsupported_scope());
    }

    Ok(())
}

/// Format a `Date` the way Node's `toISOString()` does.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `GET /v1/states/:entryId/:hash`
async fn get_state(
    State(state): State<AppState>,
    Path((raw_entry_id, hash)): Path<(String, String)>,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    ensure_entry(&state, entry_id).await?;

    let row: Option<(String, i64, Option<Value>, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
SELECT hash, entry_id, data, created_at
FROM states
WHERE hash = $1
  AND entry_id = $2
LIMIT 1
"#,
    )
    .bind(hash.as_str())
    .bind(entry_id)
    .fetch_optional(&state.db.replica)
    .await?;

    let Some((hash, entry_id, data, created_at)) = row else {
        return Err(not_exist_state_by_hash().into());
    };

    // The Node service hands the `pg`-parsed JSON to `res.send`, i.e. the body
    // is `JSON.stringify(data)` — reproduce ECMAScript number formatting.
    let data = match data {
        Some(data) => js_json(&data),
        None => "null".to_owned(),
    };

    let created_at = match created_at {
        Some(value) => js_string_literal(&format_timestamp(value)),
        None => "null".to_owned(),
    };

    let body = format!(
        r#"{{"hash":{},"entryId":{},"data":{},"createdAt":{}}}"#,
        js_string_literal(&hash),
        js_string_literal(&encode_id(entry_id)),
        data,
        created_at,
    );

    Ok((
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response())
}

/// `POST /v1/states/:entryId`
async fn create_state(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;
    let body = parse_body(&headers, &body)?;

    let map = match &body {
        Value::Object(map) => map,
        other => {
            return Err(ApiError::validation(json!([{
                "expected": "object",
                "code": "invalid_type",
                "path": [],
                "message": format!(
                    "Invalid input: expected object, received {}",
                    js_type_name(other)
                ),
            }]))
            .into())
        }
    };

    let data = match map.get("data") {
        None => {
            return Err(ApiError::validation(json!([{
                "expected": "record",
                "code": "invalid_type",
                "path": ["data"],
                "message": "Invalid input: expected record, received undefined",
            }]))
            .into())
        }
        Some(Value::Object(data)) => data,
        Some(other) => {
            return Err(ApiError::validation(json!([{
                "expected": "record",
                "code": "invalid_type",
                "path": ["data"],
                "message": format!(
                    "Invalid input: expected record, received {}",
                    js_type_name(other)
                ),
            }]))
            .into())
        }
    };

    let data = Value::Object(data.clone());

    // `zc.limitedObject`: `JSON.stringify(val).length <= limit`, where
    // `String#length` counts UTF-16 code units.
    let size = js_json(&data).encode_utf16().count();
    if size > MAX_STATE_DATA_OBJECT_SYMBOLS {
        return Err(ApiError::validation(json!([{
            "code": "custom",
            "path": ["data"],
            "params": {"code": "OBJECT_SIZE_LIMIT_EXCEEDED"},
            "message": format!(
                "Object can't contain more than {MAX_STATE_DATA_OBJECT_SYMBOLS} characters"
            ),
        }]))
        .into());
    }

    ensure_entry(&state, entry_id).await?;

    let hash = hash_generator(entry_id, &data);

    let existing: Option<(String,)> = sqlx::query_as(
        r#"
SELECT hash
FROM states
WHERE hash = $1
  AND entry_id = $2
LIMIT 1
"#,
    )
    .bind(hash.as_str())
    .bind(entry_id)
    .fetch_optional(&state.db.primary)
    .await?;

    if existing.is_some() {
        return Ok(Json(json!({ "hash": hash })).into_response());
    }

    sqlx::query(
        r#"
INSERT INTO states (hash, entry_id, data)
VALUES ($1, $2, $3)
RETURNING hash
"#,
    )
    .bind(hash.as_str())
    .bind(entry_id)
    .bind(sqlx::types::Json(js_numbers_as_f64(&data)))
    .fetch_one(&state.db.primary)
    .await?;

    Ok(Json(json!({ "hash": hash })).into_response())
}

/// Every number in the payload is an IEEE-754 double on the Node side
/// (`JSON.parse`), so store the same value in `jsonb`.
fn js_numbers_as_f64(value: &Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64().and_then(Number::from_f64) {
            Some(number) => Value::Number(number),
            None => Value::Number(Number::from(0)),
        },
        Value::Array(items) => Value::Array(items.iter().map(js_numbers_as_f64).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), js_numbers_as_f64(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected hashes were produced by the Node service
    /// (`POST /v1/states/<entryId>` against `http://127.0.0.1:18091`, backed by
    /// the very same `hash-generator.ts` + `crc-32`).
    fn node_hash(payload: &str) -> String {
        let data: Value = serde_json::from_str(payload).expect("valid json");
        hash_generator(2339622471763756276, &data)
    }

    #[test]
    fn hash_matches_node_for_simple_objects() {
        assert_eq!(node_hash(r#"{"b":2,"a":1}"#), "5764394232");
        assert_eq!(node_hash(r#"{"a":1,"b":3}"#), "4e7f080332");
        assert_eq!(node_hash(r#"{}"#), "5a5e3f1e21");
        assert_eq!(node_hash(r#"{"arr":[3,1,{"z":1,"a":2}]}"#), "a22f883346");
    }

    #[test]
    fn hash_matches_node_for_numbers_unicode_and_nulls() {
        assert_eq!(
            node_hash(
                r#"{"n":null,"u":"Привет","f":1.5,"e":1e21,"big":12345678901234567890,"neg":-0}"#
            ),
            "ff530eef95"
        );
    }

    #[test]
    fn hash_is_stable_regardless_of_key_order() {
        assert_eq!(node_hash(r#"{"b":2,"a":1}"#), node_hash(r#"{"a":1,"b":2}"#));
    }

    #[test]
    fn js_number_formatting_matches_ecmascript() {
        // Values observed with `node -p 'JSON.stringify(x)'`.
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-1.0, "-1"),
            (1.5, "1.5"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (1.2345678901234567e19, "12345678901234567000"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (123456789012345680000.0, "123456789012345680000"),
        ];

        for (value, expected) in cases {
            assert_eq!(js_number_string(*value), *expected, "value {value}");
        }
    }

    #[test]
    fn js_string_escaping_matches_json_stringify() {
        assert_eq!(js_string_literal("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(
            js_string_literal("\u{1}\u{8}\u{9}\u{a}\u{c}\u{d}"),
            r#""\u0001\b\t\n\f\r""#
        );
        assert_eq!(js_string_literal("Привет/😀"), "\"Привет/😀\"");
    }

    #[test]
    fn key_sorting_uses_utf16_code_units() {
        // U+FFFD is 0xFFFD in UTF-16, 😀 is the surrogate pair D83D DE00 —
        // `Array#sort` puts the surrogate pair first, Rust's `str` order would
        // put U+FFFD first.
        assert_eq!(js_key_cmp("\u{FFFD}", "😀"), std::cmp::Ordering::Greater);

        let value: Value = serde_json::from_str(r#"{"\uFFFD":1,"😀":2}"#).expect("valid json");
        assert_eq!(js_json_sorted(&value), "{\"😀\":2,\"\u{FFFD}\":1}");
    }

    #[test]
    fn sorted_serialization_recurses_into_arrays() {
        let value: Value =
            serde_json::from_str(r#"{"b":{"d":1,"c":2},"a":[{"z":1,"y":2}]}"#).expect("valid json");

        assert_eq!(
            js_json_sorted(&value),
            r#"{"a":[{"y":2,"z":1}],"b":{"c":2,"d":1}}"#
        );
        // Unsorted serialization (`JSON.stringify`, used by `limitedObject`)
        // keeps insertion order.
        assert_eq!(
            js_json(&value),
            r#"{"b":{"d":1,"c":2},"a":[{"z":1,"y":2}]}"#
        );
    }

    #[test]
    fn numbers_are_rounded_to_doubles_for_storage() {
        let value: Value =
            serde_json::from_str(r#"{"big":12345678901234567890}"#).expect("valid json");

        assert_eq!(
            js_json(&js_numbers_as_f64(&value)),
            r#"{"big":12345678901234567000}"#
        );
    }
}
