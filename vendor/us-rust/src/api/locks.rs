//! `src/controllers/locks/*` — verify-existence, lock, unlock, extend.
//!
//! Ports of `src/services/new/lock/*` (v1.196.0) under the flags this build
//! runs with: `AUTH_ENABLED=false` (so `accessServiceEnabled === false`) and
//! `DLS_ENABLED=false`.  With those flags `checkLockPermission` reduces to the
//! entry-existence lookup: the workbook / collection permission branches are
//! all guarded by `accessServiceEnabled`, and the DLS branch by
//! `ctx.config.dlsEnabled`.  `isPrivateRoute` is `undefined` for these routes
//! (the `resolvePrivateRoute` middleware only marks `private` routes), so every
//! lock route does run `checkLockPermission`.
//!
//! Wire contract details that are easy to get wrong:
//!   * `Lock` rows are returned as strings with `Date#toISOString()` precision
//!     (`YYYY-MM-DDTHH:MM:SS.mmmZ`), because the Node service serializes `Date`
//!     instances coming back from `pg`;
//!   * `moment().format()` — the value stored in `expiry_date` — **truncates to
//!     seconds** (its default format has no `SSS`);
//!   * `pullActiveLock` compares `expiry_date > moment().format()`, i.e. against
//!     a second-precision "now"; a lock that expired a few hundred ms ago is
//!     still invisible (it is simply not selected, never deleted);
//!   * the `uniq_active_lock_per_entry_id` exclusion violation (`23P01`) drives
//!     the `force` re-lock path and the `ENTRY_IS_LOCKED` response.

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, SubsecRound, Utc};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sqlx::postgres::PgDatabaseError;

use crate::error::ApiError;
use crate::id::{decode_id, encode_id};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    // The private routes use the very same controllers (`authPolicy: disabled`);
    // under `accessServiceEnabled = false` the permission branches that
    // `isPrivateRoute` would skip are no-ops anyway, so the handlers are shared.
    let locks = get(verify_existence).post(lock).delete(unlock);

    crate::api::dual_router(vec![
        ("/v1/locks/{entry_id}", locks.clone()),
        ("/private/locks/{entry_id}", locks),
        ("/v1/locks/{entry_id}/extend", post(extend)),
        ("/private/locks/{entry_id}/extend", post(extend)),
    ])
}

/// `src/services/new/lock/constants.ts`
const MAX_LOCK_DURATION: f64 = 600_000.0;
const DEFAULT_LOCK_DURATION: f64 = 300_000.0;

/// `const UNIQ_ACTIVE_LOCK_CONSTRAINT` of `lock-entry.ts`.
const UNIQ_ACTIVE_LOCK_CONSTRAINT: &str = "uniq_active_lock_per_entry_id";

/// `PG_ERRORS.EXCLUSION_VIOLATION`
const EXCLUSION_VIOLATION: &str = "23P01";

// ---------------------------------------------------------------------------
// errors / responses
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

fn not_exist_locked_entry() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("NOT_EXIST_LOCKED_ENTRY"),
        "The entity isn't locked",
    )
}

fn entry_is_locked(login: Option<String>, expiry_date: String) -> ApiError {
    ApiError::new(
        StatusCode::LOCKED,
        Some("ERR.US.ENTRY_IS_LOCKED"),
        "The entry is locked",
    )
    .with_details(json!({
        "loginOrId": login,
        "expiryDate": expiry_date,
    }))
}

fn entry_lock_force_conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        Some("ERR.US.ENTRY_LOCK_FORCE_CONFLICT"),
        "Conflict occurred while setting a forced lock",
    )
}

/// `LockDurationIsLimitedError({message: 'The max duration is 600000'})`
fn duration_is_limited() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("DURATION_IS_LIMITED"),
        "The max duration is 600000",
    )
}

/// `new LockTokenRequiredError()` — the canonical message.
fn lock_token_required() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("LOCK_TOKEN_REQUIRED"),
        "The lock token is required",
    )
}

/// `new LockTokenRequiredError({message: 'The correct lock token is required'})`
fn correct_lock_token_required() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        Some("LOCK_TOKEN_REQUIRED"),
        "The correct lock token is required",
    )
}

// ---------------------------------------------------------------------------
// request parsing (express body-parser + zod)
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

/// `duration: z.number().min(1).optional()`
fn parse_duration(body: &Map<String, Value>, issues: &mut Vec<Value>) -> Option<f64> {
    match body.get("duration") {
        None => None,
        Some(Value::Number(number)) => {
            let value = number.as_f64().unwrap_or(f64::NAN);

            if value.is_nan() {
                issues.push(issue_invalid_type(&["duration"], "number", "NaN"));
                None
            } else if value < 1.0 {
                issues.push(json!({
                    "origin": "number",
                    "code": "too_small",
                    "minimum": 1,
                    "inclusive": true,
                    "path": ["duration"],
                    "message": "Too small: expected number to be >=1",
                }));
                None
            } else {
                Some(value)
            }
        }
        Some(other) => {
            issues.push(issue_invalid_type(
                &["duration"],
                "number",
                js_type_name(other),
            ));
            None
        }
    }
}

/// `force: z.boolean().optional()`
fn parse_boolean_field(body: &Map<String, Value>, field: &str, issues: &mut Vec<Value>) -> bool {
    match body.get(field) {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            issues.push(issue_invalid_type(&[field], "boolean", js_type_name(other)));
            false
        }
    }
}

/// `zc.stringBoolean()`: `z.string().toLowerCase().transform(isTrueArg)` with
/// `TRUE_FLAGS = ['1', 'true', true]`.
fn is_true_arg(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true")
}

/// Flat query-string pairs (`qs` semantics for the keys we care about).
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

fn query_first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

// ---------------------------------------------------------------------------
// database helpers (`src/services/new/lock/*`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
struct LockRow {
    lock_id: i64,
    entry_id: i64,
    lock_token: String,
    expiry_date: DateTime<Utc>,
    start_date: Option<DateTime<Utc>>,
    login: Option<String>,
}

/// `lockModel.format`
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LockModel {
    lock_id: String,
    entry_id: String,
    lock_token: String,
    expiry_date: String,
    start_date: Option<String>,
    login: Option<String>,
}

impl LockRow {
    fn into_model(self) -> LockModel {
        LockModel {
            lock_id: encode_id(self.lock_id),
            entry_id: encode_id(self.entry_id),
            lock_token: self.lock_token,
            expiry_date: format_timestamp(self.expiry_date),
            start_date: self.start_date.map(format_timestamp),
            login: self.login,
        }
    }
}

/// Node serializes `Date` with `toISOString()`: UTC, milliseconds, `Z`.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `moment().format()` — second precision.
fn now_seconds() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(0)
}

/// `moment().add(duration, 'ms').format()` — second precision.
fn expiry_after(duration: f64) -> DateTime<Utc> {
    let millis = duration as i64;
    (Utc::now() + Duration::milliseconds(millis)).trunc_subsecs(0)
}

const PULL_ACTIVE_LOCK_SQL: &str = r#"
SELECT
    lock_id,
    entry_id,
    lock_token,
    expiry_date,
    start_date,
    login
FROM locks
WHERE entry_id = $1
  AND expiry_date > $2
LIMIT 1
"#;

/// `pullActiveLock`
async fn pull_active_lock(
    executor: &sqlx::PgPool,
    entry_id: i64,
) -> Result<Option<LockRow>, sqlx::Error> {
    sqlx::query_as::<_, LockRow>(PULL_ACTIVE_LOCK_SQL)
        .bind(entry_id)
        .bind(now_seconds())
        .fetch_optional(executor)
        .await
}

/// `checkLockPermission` with `accessServiceEnabled === false` and
/// `dlsEnabled === false`: only the entry lookup can fail.
async fn check_lock_permission(state: &AppState, entry_id: i64) -> Result<(), ApiError> {
    let row: Option<(i64,)> = sqlx::query_as(
        r#"
SELECT entry_id
FROM entries
WHERE entry_id = $1
  AND tenant_id = $2
  AND is_deleted = FALSE
LIMIT 1
"#,
    )
    .bind(entry_id)
    .bind(state.config.tenant_id_override.as_str())
    .fetch_optional(&state.db.replica)
    .await?;

    if row.is_none() {
        return Err(ApiError::not_exist_entry());
    }

    Ok(())
}

/// `Utils.generateLockToken()`:
/// `Math.random().toString(36).substring(2, 10)` twice (16 base-36 characters).
fn generate_lock_token() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    let mut token = String::with_capacity(16);

    for _ in 0..2 {
        let mut seed = random_u64();

        for _ in 0..8 {
            token.push(ALPHABET[(seed % 36) as usize] as char);
            seed /= 36;
        }
    }

    token
}

fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `GET /v1/locks/:entryId` — `verifyLockExistence`
async fn verify_existence(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    // `verifyLockExistence`: private routes skip the permission (entry) check.
    if !uri.path().starts_with("/private") {
        check_lock_permission(&state, entry_id).await?;
    }

    // Expired rows are never returned (and never deleted by this service):
    // `pullActiveLock` filters on `expiry_date > now`.
    match pull_active_lock(&state.db.primary, entry_id).await? {
        Some(lock) => Ok(Json(lock.into_model()).into_response()),
        None => Err(not_exist_locked_entry().into()),
    }
}

/// `POST /v1/locks/:entryId` — `lockEntry`
async fn lock(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;
    let body = parse_body(&headers, &body)?;

    let map = ensure_object_body(&body)?;

    let mut issues = Vec::new();
    let duration = parse_duration(map, &mut issues);
    let force = parse_boolean_field(map, "force", &mut issues);

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues).into());
    }

    let duration = duration.unwrap_or(DEFAULT_LOCK_DURATION);

    if duration > MAX_LOCK_DURATION {
        return Err(duration_is_limited().into());
    }

    check_lock_permission(&state, entry_id).await?;

    let lock_token = generate_lock_token();
    let expiry_date = expiry_after(duration);
    let login = "unknown".to_owned();

    let insert = sqlx::query_as::<_, LockRow>(
        r#"
INSERT INTO locks (entry_id, lock_token, expiry_date, login)
VALUES ($1, $2, $3, $4)
RETURNING lock_id, entry_id, lock_token, expiry_date, start_date, login
"#,
    )
    .bind(entry_id)
    .bind(lock_token.as_str())
    .bind(expiry_date)
    .bind(login.as_str())
    .fetch_one(&state.db.primary)
    .await;

    let row = match insert {
        Ok(row) => row,
        Err(error) => {
            if !is_active_lock_conflict(&error) {
                return Err(error.into());
            }

            let locked_entry = pull_active_lock(&state.db.primary, entry_id).await?;

            match locked_entry {
                Some(locked_entry) if force => {
                    let updated = sqlx::query_as::<_, LockRow>(
                        r#"
UPDATE locks
SET lock_token = $1,
    start_date = CURRENT_TIMESTAMP,
    expiry_date = $2,
    login = $3
WHERE entry_id = $4
  AND lock_token = $5
RETURNING lock_id, entry_id, lock_token, expiry_date, start_date, login
"#,
                    )
                    .bind(lock_token.as_str())
                    .bind(expiry_date)
                    .bind(login.as_str())
                    .bind(entry_id)
                    .bind(locked_entry.lock_token.as_str())
                    .fetch_optional(&state.db.primary)
                    .await?;

                    match updated {
                        Some(row) => row,
                        None => return Err(entry_lock_force_conflict().into()),
                    }
                }
                Some(locked_entry) => {
                    return Err(entry_is_locked(
                        locked_entry.login,
                        format_timestamp(locked_entry.expiry_date),
                    )
                    .into());
                }
                None => return Err(error.into()),
            }
        }
    };

    Ok(Json(json!({ "lockToken": row.lock_token })).into_response())
}

/// `error instanceof DBError && code === EXCLUSION_VIOLATION && constraint === …`
fn is_active_lock_conflict(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(database_error) => {
            let is_exclusion = database_error.code().as_deref() == Some(EXCLUSION_VIOLATION);

            let constraint = database_error
                .try_downcast_ref::<PgDatabaseError>()
                .and_then(|postgres_error| postgres_error.constraint());

            is_exclusion && constraint == Some(UNIQ_ACTIVE_LOCK_CONSTRAINT)
        }
        _ => false,
    }
}

/// `DELETE /v1/locks/:entryId` — `unlockEntry`
async fn unlock(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    let pairs = parse_query(query.as_deref());
    let lock_token = query_first(&pairs, "lockToken").map(ToOwned::to_owned);
    let force = query_first(&pairs, "force")
        .map(is_true_arg)
        .unwrap_or(false);

    if lock_token.is_none() && !force {
        return Err(correct_lock_token_required().into());
    }

    check_lock_permission(&state, entry_id).await?;

    let mut transaction = state.db.primary.begin().await?;

    let locked_entry: Option<LockRow> = sqlx::query_as::<_, LockRow>(PULL_ACTIVE_LOCK_SQL)
        .bind(entry_id)
        .bind(now_seconds())
        .fetch_optional(&mut *transaction)
        .await?;

    let locked_entry = match locked_entry {
        Some(row) => row,
        None => {
            transaction.rollback().await.ok();
            return Err(not_exist_locked_entry().into());
        }
    };

    if let Some(token) = lock_token.as_deref() {
        if token != locked_entry.lock_token {
            transaction.rollback().await.ok();
            return Err(lock_token_required().into());
        }
    }

    let deleted: Option<LockRow> = sqlx::query_as::<_, LockRow>(
        r#"
DELETE FROM locks
WHERE entry_id = $1
  AND lock_token = $2
RETURNING lock_id, entry_id, lock_token, expiry_date, start_date, login
"#,
    )
    .bind(locked_entry.entry_id)
    .bind(locked_entry.lock_token.as_str())
    .fetch_optional(&mut *transaction)
    .await?;

    transaction.commit().await?;

    match deleted {
        Some(row) => Ok(Json(row.into_model()).into_response()),
        None => Ok(StatusCode::OK.into_response()),
    }
}

/// `POST /v1/locks/:entryId/extend` — `extendLock`
async fn extend(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HandlerError> {
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;
    let body = parse_body(&headers, &body)?;

    let map = ensure_object_body(&body)?;

    let mut issues = Vec::new();
    let duration = parse_duration(map, &mut issues);
    let force = parse_boolean_field(map, "force", &mut issues);
    let lock_token = match map.get("lockToken") {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(
                &["lockToken"],
                "string",
                js_type_name(other),
            ));
            None
        }
    };

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues).into());
    }

    let duration = duration.unwrap_or(DEFAULT_LOCK_DURATION);

    if duration > MAX_LOCK_DURATION {
        return Err(duration_is_limited().into());
    }

    if !force && lock_token.is_none() {
        return Err(lock_token_required().into());
    }

    check_lock_permission(&state, entry_id).await?;

    let expiry_date = expiry_after(duration);

    let mut transaction = state.db.primary.begin().await?;

    let locked_entry: Option<LockRow> = sqlx::query_as::<_, LockRow>(PULL_ACTIVE_LOCK_SQL)
        .bind(entry_id)
        .bind(now_seconds())
        .fetch_optional(&mut *transaction)
        .await?;

    let locked_entry = match locked_entry {
        Some(row) => row,
        None => {
            transaction.rollback().await.ok();
            return Err(not_exist_locked_entry().into());
        }
    };

    if let Some(token) = lock_token.as_deref() {
        if token != locked_entry.lock_token {
            transaction.rollback().await.ok();
            return Err(lock_token_required().into());
        }
    }

    let updated: Option<LockRow> = sqlx::query_as::<_, LockRow>(
        r#"
UPDATE locks
SET expiry_date = $1,
    login = $2
WHERE entry_id = $3
  AND lock_token = $4
RETURNING lock_id, entry_id, lock_token, expiry_date, start_date, login
"#,
    )
    .bind(expiry_date)
    .bind("unknown")
    .bind(entry_id)
    .bind(locked_entry.lock_token.as_str())
    .fetch_optional(&mut *transaction)
    .await?;

    transaction.commit().await?;

    match updated {
        Some(row) => Ok(Json(row.into_model()).into_response()),
        None => Ok(StatusCode::OK.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(seconds: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(seconds)
            .expect("rfc3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn lock_model_timestamps_match_js_to_iso_string() {
        // `2026-11-03T23:25:10.703Z` is what Node returns for a startDate with
        // milliseconds; `expiryDate` always lands on `.000Z` because
        // `moment().format()` truncates.
        let row = LockRow {
            lock_id: 2339622471763756276,
            entry_id: 2339622471763756276,
            lock_token: "uw2n6jpjx9euyb02".to_owned(),
            expiry_date: at("2026-11-03T23:25:06Z"),
            start_date: Some(at("2026-11-03T23:25:01.656Z")),
            login: Some("unknown".to_owned()),
        };

        let model = row.into_model();

        assert_eq!(model.entry_id, encode_id(2339622471763756276));
        assert_eq!(model.expiry_date, "2026-11-03T23:25:06.000Z");
        assert_eq!(
            model.start_date.as_deref(),
            Some("2026-11-03T23:25:01.656Z")
        );
    }

    #[test]
    fn expiry_is_truncated_to_seconds() {
        let now = Utc::now();
        let expiry = expiry_after(5000.0);

        assert_eq!(expiry.timestamp_subsec_millis(), 0);
        assert!(expiry > now.trunc_subsecs(0));
    }

    #[test]
    fn lock_token_is_sixteen_base36_chars() {
        for _ in 0..16 {
            let token = generate_lock_token();

            assert_eq!(token.len(), 16);
            assert!(token
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch.is_ascii_lowercase()));
        }
    }

    #[test]
    fn true_flags_match_string_boolean() {
        assert!(is_true_arg("1"));
        assert!(is_true_arg("TRUE"));
        assert!(!is_true_arg("yes"));
        assert!(!is_true_arg(""));
    }

    #[test]
    fn timestamp_of_epoch_is_utc() {
        let value = Utc.timestamp_opt(0, 0).unwrap();
        assert_eq!(format_timestamp(value), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn query_pairs_decode_brackets() {
        let pairs = parse_query(Some("lockToken=abc&force=true"));
        assert_eq!(query_first(&pairs, "lockToken"), Some("abc"));
        assert_eq!(query_first(&pairs, "force"), Some("true"));
    }
}
