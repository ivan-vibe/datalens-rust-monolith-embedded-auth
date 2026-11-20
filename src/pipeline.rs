//! The /api/run pipeline: config + params -> data-api request -> the Rust chart
//! builder -> response envelope. It mirrors the observable contract of
//! charts-engine/components/processor/index.js without running any JavaScript.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;

use crate::engine_main::AppState;

/// `DL_RUST_DISABLE` gates every Rust builder. It is deployment-static, but the
/// dispatch used to read the environment (and build a `String`) up to four times per
/// `/api/run`; cache the decision for the process lifetime.
fn rust_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var("DL_RUST_DISABLE").is_ok())
}

#[derive(Clone)]
struct RequestScope {
    tenant: String,
    user: String,
    branch: String,
    rev: String,
    cache_allowed: bool,
    headers: axum::http::HeaderMap,
}

impl RequestScope {
    fn cache_key(&self, dataset: &str) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.tenant, self.user, self.branch, self.rev, dataset
        )
    }
}


/// The UI stores the access token in the httpOnly `auth` cookie: URL-encoded JSON
/// `{"accessToken": "...", "refreshToken": "..."}`. `/api/run` is proxied by nginx
/// straight to this service, so the cookie is the only credential the browser sends;
/// data-api needs it as `Authorization: Bearer`.
fn access_token_from_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())?;
    let raw = cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("auth=").map(str::to_string))?;
    let decoded = percent_decode(&raw);
    let value: Value = serde_json::from_str(&decoded).ok()?;
    value
        .get("accessToken")
        .and_then(|token| token.as_str())
        .map(str::to_string)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn header_hash(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let value = headers.get(name).and_then(|value| value.to_str().ok())?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    Some(format!("{:x}", hasher.finish()))
}

/// The tenant of this deployment, not of the caller.
///
/// `x-dl-tenantid` is a client header: using it verbatim (in cache keys, forwarded US reads)
/// lets a caller pin entries into another tenant's key space. This opensource build has a
/// single effective tenant (`TENANT_ID_OVERRIDE`, pinned by the deployment), so a header that
/// disagrees with it is rejected instead of trusted. With auth disabled (local lab) the header
/// is still ignored for keys: the deployment tenant always wins.
fn validate_tenant(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, String)> {
    let Some(value) = headers
        .get("x-dl-tenantid")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    if value == state.tenant {
        return Ok(());
    }
    if !state.auth_enabled {
        // Lab profile: no tenant isolation to enforce; ignore the header rather than fail.
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        format!(
            "ERR.DS_API.REQUEST.TENANT_MISMATCH: tenant {value:?} is not this deployment's tenant"
        ),
    ))
}

fn build_scope(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    raw_body: &Value,
) -> RequestScope {
    // Effective tenant is deployment-static; see `validate_tenant`.
    let tenant = state.tenant.clone();
    let user = header_hash(headers, "authorization")
        .or_else(|| header_hash(headers, "cookie"))
        .unwrap_or_else(|| "anon".to_string());
    let branch = headers
        .get("x-dl-branch")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| state.branch.clone());
    let rev = headers
        .get("x-dl-rev-id")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string())
        .or_else(|| {
            raw_body
                .get("revId")
                .or_else(|| raw_body.get("revisionId"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .unwrap_or_default();
    let edit_mode = raw_body
        .get("isEditMode")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        || raw_body
            .get("editMode")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        || raw_body
            .get("unreleased")
            .map(|value| value == "1" || value == true)
            .unwrap_or(false)
        || headers.contains_key("x-dl-edit-mode");
    let cache_allowed = state.fields_ttl.as_secs() > 0
        && !edit_mode
        && (!state.auth_enabled || state.fields_cache_auth);
    let mut forward = axum::http::HeaderMap::with_capacity(9);
    // Tenant is not forwarded from the request: the deployment tenant is authoritative and
    // downstream US/data-api reads must not see a spoofed value.
    if let Ok(value) = axum::http::HeaderValue::from_str(&state.tenant) {
        forward.insert(axum::http::header::HeaderName::from_static("x-dl-tenantid"), value);
    }
    for name in [
        "authorization",
        "cookie",
        "x-dl-allow-superuser",
        "x-dl-sudo",
        "x-dl-context",
        "x-request-id",
        "x-chart-id",
        "x-dl-component",
    ] {
        if let Some(value) = headers.get(name) {
            forward.insert(
                axum::http::header::HeaderName::from_static(name),
                value.clone(),
            );
        }
    }
    if !forward.contains_key(axum::http::header::AUTHORIZATION) {
        if let Some(token) = access_token_from_cookie(&forward) {
            if let Ok(value) = axum::http::HeaderValue::from_str(&format!("Bearer {token}")) {
                forward.insert(axum::http::header::AUTHORIZATION, value);
            }
        }
    }
    static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(1);
    // `format!` allocated a `String` per request just to build this header; write it into a
    // stack buffer instead (same value, no heap allocation).
    let mut snapshot = [0u8; 32];
    let snapshot_total = snapshot.len();
    let snapshot_len = {
        use std::io::Write as _;
        let mut cursor = &mut snapshot[..];
        match cursor.write_fmt(format_args!(
            "{}-{}",
            std::process::id(),
            SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed)
        )) {
            Ok(()) => snapshot_total - cursor.len(),
            Err(_) => 0,
        }
    };
    if snapshot_len > 0 {
        if let Ok(value) = axum::http::HeaderValue::from_bytes(&snapshot[..snapshot_len]) {
            forward.insert("x-dl-snapshot", value);
        }
    }
    RequestScope {
        tenant,
        user,
        branch,
        rev,
        cache_allowed,
        headers: forward,
    }
}

#[cfg(test)]
mod auth_cookie_tests {
    use super::*;

    #[test]
    fn the_auth_cookie_becomes_a_bearer_header() {
        let cookie =
            "%7B%22accessToken%22%3A%22header.payload.sig%22%2C%22refreshToken%22%3A%22r%22%7D";
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("other=x; auth={cookie}; auth_exp=1")
                .parse()
                .unwrap(),
        );
        assert_eq!(
            access_token_from_cookie(&headers).as_deref(),
            Some("header.payload.sig")
        );
    }

    #[test]
    fn a_missing_or_bad_cookie_has_no_token() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(access_token_from_cookie(&headers), None);
        headers.insert(axum::http::header::COOKIE, "auth=not-json".parse().unwrap());
        assert_eq!(access_token_from_cookie(&headers), None);
    }
}

fn fields_cached(state: &Arc<AppState>, key: &str) -> Option<(Arc<Value>, Arc<Value>)> {
    let cache = state.fields_cache.lock().ok()?;
    let (stamp, fields, revision) = cache.get(key)?;
    if stamp.elapsed() < state.fields_ttl {
        // `Arc` clones only the pointer: a dashboard's 21 charts share one parsed schema.
        Some((Arc::clone(fields), Arc::clone(revision)))
    } else {
        None
    }
}

fn fields_store(state: &Arc<AppState>, key: String, fields: Arc<Value>, revision: Arc<Value>) {
    let Ok(mut cache) = state.fields_cache.lock() else {
        return;
    };
    if cache.len() >= state.fields_max && !cache.contains_key(&key) {
        let ttl = state.fields_ttl;
        cache.retain(|_, (stamp, _, _)| stamp.elapsed() < ttl);
        if cache.len() >= state.fields_max {
            if let Some(oldest) = cache.keys().next().cloned() {
                cache.remove(&oldest);
            }
        }
    }
    cache.insert(key, (std::time::Instant::now(), fields, revision));
}


// ---------------------------------------------------------------------------------------------
// v8: strict in-flight single-flight for dataset `/fields` (recovered coalescing, no cache)
// ---------------------------------------------------------------------------------------------
//
// v5.8 removed the old 250 ms `FIELDS_BURST` window (it could serve a schema/revision from a
// finished request to a later one). That made the default TTL=0 path strictly fresh but lost
// coalescing: a dashboard's widgets all re-read the same `/fields` on load, and the mock
// track pays a loopback data-api round trip per widget. This is the 100%-safe middle ground:
//
// * only CONCURRENTLY EXECUTING identical reads share one fetch — the shared value is the
//   same request-time snapshot, produced while every sharer is still waiting;
// * the `done` flag is set by the fetch future before the value is published and the map
//   entry is removed immediately after completion; a request that arrives after completion
//   sees `done` (or no entry) and starts a fresh read — **no value is reused after the
//   response, no TTL window exists**;
// * cancellation is safe: tokio's `OnceCell::get_or_init` leaves the cell uninitialized when
//   its initializer is dropped, so a waiter (or a later request) retries the fetch — nobody
//   hangs;
// * memory is bounded by the 1024-entry safety valve (entries are removed on completion;
//   the valve only matters if an initializer is cancelled before its cleanup).
//
// `V51_FIELDS_BURST_MS` is kept as an on/off gate only (`0` disables coalescing; any other
// or unset value enables strict single-flight). The old time-window semantics are gone for
// good — do not reintroduce a TTL here.
type FieldsResult = Result<(Arc<Value>, Arc<Value>), (StatusCode, String)>;

struct FieldsFlight {
    cell: tokio::sync::OnceCell<FieldsResult>,
    /// Set by the initializer just before it produces the value: a caller that observes
    /// `done == true` must not join this flight, it must start its own fetch.
    done: AtomicBool,
}

impl FieldsFlight {
    fn new() -> Self {
        Self {
            cell: tokio::sync::OnceCell::new(),
            done: AtomicBool::new(false),
        }
    }
}

static FIELDS_FLIGHTS: std::sync::OnceLock<
    tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<FieldsFlight>>>,
> = std::sync::OnceLock::new();

fn fields_flights() -> &'static tokio::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<FieldsFlight>>,
> {
    FIELDS_FLIGHTS.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()))
}

fn fields_coalescing_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("V51_FIELDS_BURST_MS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok()),
            Some(0)
        )
    })
}

async fn fetch_fields(
    state: &Arc<AppState>,
    scope: &RequestScope,
    dataset: &str,
) -> Result<(Arc<Value>, Arc<Value>), (StatusCode, String)> {
    crate::data_direct::fetch_fields(&state.http, &state.data_api, &scope.headers, dataset)
        .await
        .map(|(fields, revision)| (Arc::new(fields), Arc::new(revision)))
}

/// Dataset ids of a chart config in any version: `datasetsIds` (v2+) or the V1
/// `datasets[].id` / singular `dataset.id` that `mapV1ConfigToV2` migrates from.
fn shared_dataset_ids(shared: &Value) -> Vec<String> {
    if let Some(items) = shared.get("datasetsIds").and_then(Value::as_array) {
        let ids: Vec<String> = items
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
        if !ids.is_empty() {
            return ids;
        }
    }
    if let Some(items) = shared.get("datasets").and_then(Value::as_array) {
        let ids: Vec<String> = items
            .iter()
            .filter_map(|dataset| dataset.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            return ids;
        }
    }
    shared
        .pointer("/dataset/id")
        .and_then(Value::as_str)
        .map(|id| vec![id.to_string()])
        .unwrap_or_default()
}


async fn dataset_fields(
    state: &Arc<AppState>,
    scope: &RequestScope,
    dataset: &str,
) -> Result<(Arc<Value>, Arc<Value>), (StatusCode, String)> {
    let key = scope.cache_key(dataset);
    if let Some(cached) = fields_cached(state, &key) {
        state
            .fields_hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Ok(cached);
    }
    state
        .fields_misses
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !scope.cache_allowed {
        if !fields_coalescing_enabled() {
            // Pure no-cache/no-coalescing diagnostic mode.
            state
                .fields_fetches
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return fetch_fields(state, scope, dataset).await;
        }
        // Strict single-flight: join the in-flight read for this key, or become its leader.
        // The map lock is only held for the lookup, never across the fetch.
        let flight = {
            let mut flights = fields_flights().lock().await;
            if flights.len() >= 1024 {
                flights.clear();
            }
            match flights.get(&key) {
                Some(existing) if !existing.done.load(Ordering::Acquire) => existing.clone(),
                _ => {
                    let flight = std::sync::Arc::new(FieldsFlight::new());
                    flights.insert(key.clone(), flight.clone());
                    flight
                }
            }
        };
        let initializer = std::sync::Arc::clone(&flight);
        let result = flight
            .cell
            .get_or_init(|| async move {
                state
                    .fields_fetches
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let result = fetch_fields(state, scope, dataset).await;
                // Publish completion before the cell stores the value: a caller that
                // observes `done` after this point starts a fresh read instead of reusing
                // this one.
                initializer.done.store(true, Ordering::Release);
                result
            })
            .await
            .clone();
        // Remove the entry immediately; only if it is still *this* flight (a concurrent
        // caller may already have replaced it with a fresh one).
        {
            let mut flights = fields_flights().lock().await;
            if flights
                .get(&key)
                .map(|current| std::sync::Arc::ptr_eq(current, &flight))
                .unwrap_or(false)
            {
                flights.remove(&key);
            }
        }
        return result;
    }
    let flight = {
        let mut flights = state.fields_flight.lock().await;
        flights
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let guard = flight.lock().await;
    if let Some(cached) = fields_cached(state, &key) {
        state
            .fields_hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        drop(guard);
        let mut flights = state.fields_flight.lock().await;
        if Arc::strong_count(&flight) <= 2 {
            flights.remove(&key);
        }
        return Ok(cached);
    }
    state
        .fields_fetches
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = fetch_fields(state, scope, dataset).await;
    if let Ok((fields, revision)) = &result {
        fields_store(
            state,
            key.clone(),
            Arc::clone(fields),
            Arc::clone(revision),
        );
    }
    drop(guard);
    let mut flights = state.fields_flight.lock().await;
    if flights
        .get(&key)
        .map(|entry| Arc::strong_count(entry) <= 2)
        .unwrap_or(false)
    {
        flights.remove(&key);
    }
    result
}

const SYSTEM_PALETTES: &str = concat!(
    r##"{"default20":{"id":"default20","scheme":["#3D97F2","#FF6A59","#7FD169","#FFB433","#AB94D9","#8CD5FF","#FF8CB2","#60A196","#4C73BF","#B8C9CC","#52CCC2","#C3E55C","#807D73","#FF8C00","#FFDC73","#CC9966","#CC5252","#FFB28C","#9661FF","#D4BFFF"],"datalens":true},"neutral20":{"id":"neutral20","scheme":["#EAEAEA","#E3E3E3","#DBDBDB","#D3D3D3","#CCCCCC","#C4C4C4","#BCBCBC","#B5B5B5","#ADADAD","#A6A6A6","#9E9E9E","#969696","#8F8F8F","#878787","#7F7F7F","#787878","#707070","#686868","#616161","#595959"],"gradient":true},"oceanic20":{"id":"oceanic20","scheme":["#AEF8FE","#9FF2FE","#90EAFE","#81E1FE","#71D6FE","#62CAFE","#53BCFE","#44ADFE","#359DFD","#258AFD","#1677FD","#0762FD","#024FF3","#023EE4","#0230D5","#0222C5","#0117B6","#010DA7","#010498","#050189"],"gradient":true},"golden20":{"id":"golden20","scheme":["#FDEC4D","#FDDE43","#FDD039","#FDBF2F","#FDAE25","#FD9B1B","#FD8711","#FD7207","#F75C0"##,
    r##"2","#ED4902","#E33702","#D92602","#CF1702","#C40802","#BA0208","#B00213","#A6021D","#9C0125","#92012D","#880132"],"gradient":true},"emerald20":{"id":"emerald20","scheme":["#C2EEBA","#AEEAAA","#99E69A","#89E192","#79DD8B","#68D986","#58D583","#48D183","#37CD84","#30C086","#2EB887","#2CAF88","#2AA788","#289F87","#269786","#248F84","#228782","#207E7E","#1E7276","#1C666E"],"gradient":true},"traffic-light9":{"id":"traffic-light9","scheme":["#9C0125","#F17A03","#228782","#C40802","#FDA816","#2EB887","#ED4902","#FDCE34","#79DD8B"],"gradient":true},"classic20":{"id":"classic20","scheme":["#4DA2F1","#FF3D64","#8AD554","#FFC636","#FFB9DD","#84D1EE","#FF91A1","#54A520","#DB9100","#BA74B3","#1F68A9","#ED65A9","#0FA08D","#FF7E00","#E8B0A4","#52A6C5","#BE2443","#70C1AF","#FFB46C","#DCA3D7"],"datalens":true},"datalens-neo-20":{"id":"datalens-neo-20","scheme":["#DB9101","#4DA2F1","#8AD554","#BA74B3","#F"##,
    r##"F3D64","#FFC636","#0CA18C","#DBA2D7","#BF2543","#84D1EE","#6B6B6B","#EEB184","#1E69A9","#FF7E00","#54A520","#B9B0AC","#58A9C8","#FFB46C","#70C1AF","#FF90A1"],"datalens":true}}"##,
);

/// Parsed once per process: the palette table is static, request-independent config.
/// Re-parsing it built a ~2 KB `Value` with ~150 string allocations on every request.
fn system_palettes() -> &'static Value {
    static PALETTES: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PALETTES.get_or_init(|| serde_json::from_str(SYSTEM_PALETTES).unwrap_or_else(|_| json!({})))
}

/// Mirrors `JSON.stringify` for a JS `Number`: integral doubles stay integers, non-finite values
/// become `null` (metric values arrive from data-api as numeric strings).
fn metric_json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        return json!(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn normalized_params(raw: Option<&Value>) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(object) = raw.and_then(|value| value.as_object()) {
        for (key, value) in object {
            map.insert(
                key.clone(),
                if value.is_array() {
                    value.clone()
                } else {
                    json!([value])
                },
            );
        }
    }
    Value::Object(map)
}

fn update_params(params: &mut Value, used: &mut Value, overrides: &Value) {
    let (Some(params), Some(used), Some(overrides)) = (
        params.as_object_mut(),
        used.as_object_mut(),
        overrides.as_object(),
    ) else {
        return;
    };
    for (key, value) in overrides {
        let current = params.get(key).cloned().unwrap_or(Value::Null);
        let current_has_value = match &current {
            Value::Array(items) => !items.is_empty(),
            Value::String(text) => !text.is_empty(),
            Value::Null => false,
            _ => true,
        };
        let replacement = if current_has_value {
            let override_has_value = value
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false);
            if override_has_value {
                Some(value.clone())
            } else {
                None
            }
        } else {
            Some(value.clone())
        };
        if let Some(replacement) = replacement {
            params.insert(key.clone(), replacement);
        }
        used.insert(key.clone(), params.get(key).cloned().unwrap_or(Value::Null));
    }
}

/// A param is "not set" when it is missing, `null`, or an empty array/string.
fn param_blank(params: &serde_json::Map<String, Value>, key: &str) -> bool {
    match params.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(Value::String(text)) => text.is_empty(),
        _ => false,
    }
}

/// `newParams[fieldId] = []` for every merged dataset field, then
/// `ChartEditor.updateParams` -> `userParamsOverride` (`processor/index.ts:464-506`).
/// The Params-tab sandbox is not available in Rust; this is the same emulation the pivot
/// builder already uses (`pivot_engine.rs:3412-3460`). Request values win, defaults are
/// `[]`, so `params`/`usedParams` carry one key per field instead of echoing the request.
fn chart_params(shared: &Value, params: &Value, response_fields: &[&[Value]]) -> (Value, Value) {
    // `buildGraphPrivate` fills `newParams` from **merged data-api response fields**
    // (`field.guid || field.id`), including the `__@placeholder__` template field — not from the
    // dataset schema. That is why unused dataset parameters do not appear in `params`.
    let mut overrides = serde_json::Map::new();
    for fields in response_fields {
        for field in fields.iter() {
            let id = field
                .get("guid")
                .or_else(|| field.get("id"))
                .and_then(Value::as_str);
            if let Some(id) = id {
                overrides
                    .entry(id.to_string())
                    .or_insert_with(|| json!([]));
            }
        }
    }
    let mut final_params = params.clone();
    let mut scratch = json!({});
    update_params(&mut final_params, &mut scratch, &Value::Object(overrides));
    // A chart that *uses* a hierarchy in a placeholder starts at drill-down level 0; the Python
    // processor puts those defaults into `params` — but only when the request does not already
    // carry the drill-down state. A drilled chart forwards the frontend's values untouched
    // (`drillDownLevel: ["1"]`, `drillDownFilters: ["Jaffa", ""]`) and only gains
    // `isColorDrillDown: false`. A merely declared hierarchy that is not on an axis adds nothing.
    let uses_hierarchy = shared
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|placeholder| {
            placeholder
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| item.get("data_type").and_then(Value::as_str) == Some("hierarchy"))
        });
    if let Some(hierarchy) = shared
        .get("hierarchies")
        .and_then(Value::as_array)
        .and_then(|hierarchies| hierarchies.first())
        .filter(|_| uses_hierarchy)
    {
        let levels = hierarchy
            .get("fields")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if levels > 0 {
            if let Some(object) = final_params.as_object_mut() {
                if param_blank(object, "drillDownLevel") {
                    object.insert("drillDownLevel".to_string(), json!(0));
                }
                if param_blank(object, "drillDownFilters") {
                    object.insert("drillDownFilters".to_string(), json!(vec![""; levels]));
                }
                if param_blank(object, "isColorDrillDown") {
                    object.insert("isColorDrillDown".to_string(), json!(false));
                }
            }
        }
    }
    // `usedParams` is the data-parameter subset of `params`: one key per merged field (in merged
    // order) plus the hierarchy drill controls. Interaction controls (`_columnId`, `_page`, …) and
    // request-only parameter ids that are not dataset fields are not listed.
    let mut used = serde_json::Map::new();
    for fields in response_fields {
        for field in fields.iter() {
            let Some(id) = field
                .get("guid")
                .or_else(|| field.get("id"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if used.contains_key(id) {
                continue;
            }
            let value = final_params.get(id).cloned().unwrap_or_else(|| json!([]));
            used.insert(id.to_string(), value);
        }
    }
    for key in ["drillDownLevel", "drillDownFilters", "isColorDrillDown"] {
        if let Some(value) = final_params.get(key) {
            used.insert(key.to_string(), value.clone());
        }
    }
    (final_params, Value::Object(used))
}

/// `extra.datasets` + `extra.colors` (+ `dataExportForbidden`) as the JS processor builds
/// them: `ChartEditor.setExtra('datasets', datasetsMeta)` from `plugins/datalens/js/js.ts`
/// and `selectServerPalette` in `processor/index.ts:885-910`.
fn chart_extra(
    state: &AppState,
    shared: &Value,
    datasets: &Value,
    data_export_forbidden: bool,
) -> Value {
    // `datasets` is the data-api response-field view per source:
    // `[{"id": <datasetId>, "fields": <result fields>}, ...]`.
    let mut extra = crate::pivot_engine::build_extra(shared, datasets);

    if let Some(colors) = palette_scheme(state) {
        extra["colors"] = colors;
    }
    if data_export_forbidden {
        extra["dataExportForbidden"] = json!(true);
    }
    extra
}

/// Revision-keyed cache for `extra` (metadata only: field titles/formatting and palette).
/// The key includes branch, chart entry, chart revision, dataset id + dataset revision and
/// data-export permission, so a schema/chart change is a cache miss. Access control is still
/// enforced by the entry read that supplied `shared`.
// Process-wide render caches. Module scope (not function-local) so the idle memory trimmer can
// drop them: parsed chart config/metadata dominates the warm footprint.
static CHART_EXTRA_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Value>>>,
> = std::sync::OnceLock::new();
static PARSED_SHARED_CACHE: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<
            (String, String, String),
            (std::time::Instant, std::sync::Arc<Value>),
        >,
    >,
> = std::sync::OnceLock::new();
static TABLE_REQUEST_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Value>>>,
> = std::sync::OnceLock::new();

/// Drop the parsed-config/metadata caches (idle trimmer). The entries are rebuilt on demand;
/// the revision tokens still guarantee freshness. Returns the number of dropped entries.
pub fn clear_idle_caches() -> usize {
    let mut dropped = 0;
    if let Some(cache) = CHART_EXTRA_CACHE.get() {
        if let Ok(mut cache) = cache.lock() {
            dropped += cache.len();
            cache.clear();
        }
    }
    if let Some(cache) = PARSED_SHARED_CACHE.get() {
        if let Ok(mut cache) = cache.lock() {
            dropped += cache.len();
            cache.clear();
        }
    }
    if let Some(cache) = TABLE_REQUEST_CACHE.get() {
        if let Ok(mut cache) = cache.lock() {
            dropped += cache.len();
            cache.clear();
        }
    }
    dropped
}

fn chart_extra_cached(
    state: &AppState,
    shared: &Value,
    datasets: &Value,
    data_export_forbidden: bool,
    cache_key: Option<&str>,
) -> std::sync::Arc<Value> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static MAX: OnceLock<usize> = OnceLock::new();
    /// `CHART_EXTRA_CACHE_MAX` (default 512) bounds this metadata cache; 0 disables it.
    let max = *MAX.get_or_init(|| {
        std::env::var("CHART_EXTRA_CACHE_MAX")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(512)
    });
    if max == 0 {
        return Arc::new(chart_extra(state, shared, datasets, data_export_forbidden));
    }
    let Some(key) = cache_key else {
        return Arc::new(chart_extra(state, shared, datasets, data_export_forbidden));
    };
    // `extra` also depends on the *request shape*: a drilled chart asks the data-api for the
    // current drill level's fields (and totals), so `fieldsList` follows the merged field order
    // of that request. Keying on the chart revision alone made an un-drilled request populate the
    // cache and a later drill-down reuse it, leaving `fieldsList` on the level-0 order.
    let signature = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for dataset in datasets.as_array().into_iter().flatten() {
            dataset
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .hash(&mut hasher);
            let fields = dataset.get("fields").and_then(Value::as_array);
            fields.map(Vec::len).unwrap_or(0).hash(&mut hasher);
            for field in fields.into_iter().flatten() {
                field
                    .get("guid")
                    .or_else(|| field.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .hash(&mut hasher);
                field
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .hash(&mut hasher);
                field
                    .get("data_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .hash(&mut hasher);
            }
        }
        hasher.finish()
    };
    let key = format!("{key}|{signature:016x}");
    let key = key.as_str();
    let cache = CHART_EXTRA_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(value) = cache.get(key) {
            return Arc::clone(value);
        }
    }
    let value = Arc::new(chart_extra(state, shared, datasets, data_export_forbidden));
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= max && !cache.contains_key(key) {
            if let Some(oldest) = cache.keys().next().cloned() {
                cache.remove(&oldest);
            }
        }
        cache.insert(key.to_string(), Arc::clone(&value));
    }
    value
}

fn palette_scheme(state: &AppState) -> Option<Value> {
    system_palettes()
        .get(state.default_color_palette.as_str())
        .and_then(|palette| palette.get("scheme"))
        .filter(|scheme| scheme.as_array().map(|items| !items.is_empty()).unwrap_or(false))
        .cloned()
}

/// `source.info.query` from a parsed data-api answer (`processor/index.ts:885-910`).
fn blocks_query(body: &Value) -> String {
    body.get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("query").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// The same query for the typed in-process data-api body.
fn typed_blocks_query(body: &crate::typed_data::DataBody) -> String {
    body.as_result()
        .map(|result| {
            result
                .blocks()
                .iter()
                .filter_map(|block| block.query.as_deref())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn source_key_for(dataset: &str) -> String {
    format!("{dataset}_result")
}

/// The `{error: {...}}` envelope `processor/index.ts:611-644` returns for a failed data
/// source. The message is the serialized envelope; `ui_run_inner` answers it verbatim.
fn data_fetch_error(
    source_key: &str,
    status: u16,
    request_body: &Value,
    body: &Value,
) -> (StatusCode, String) {
    let code = match status {
        403 => "ENTRY_FORBIDDEN",
        404 => "ENTRY_NOT_FOUND",
        _ => "ERR.CHARTS.DATA_FETCHING_ERROR",
    };
    let envelope = json!({
        "error": {
            "code": code,
            "details": {
                "sources": {
                    source_key: {
                        "sourceType": "bi_datasets",
                        "status": status,
                        "data": request_body,
                        "body": body,
                    }
                }
            },
        }
    });
    (
        StatusCode::from_u16(427).unwrap_or(StatusCode::BAD_REQUEST),
        serde_json::to_string(&envelope)
            .unwrap_or_else(|_| "{\"error\":{\"code\":\"ERR.CHARTS.DATA_FETCHING_ERROR\"}}".to_string()),
    )
}

/// Oversize envelope (`getOversizeError`, `js/helpers/errors/oversize-error`): HTTP 422.
/// Turn a builder failure into the client-visible answer: a structured `{error: ...}`
/// message keeps its own status (427 data-fetching, 422 oversize); anything else is
/// reported as the JS `ERR.CHARTS.DATA_FETCHING_ERROR` envelope instead of a bare 501.
fn error_response(status: StatusCode, message: &str) -> axum::response::Response {
    if let Ok(envelope) = serde_json::from_str::<Value>(message) {
        if envelope.get("error").is_some() {
            let status = envelope
                .pointer("/error/statusCode")
                .and_then(Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .and_then(|code| StatusCode::from_u16(code).ok())
                .unwrap_or(status);
            return (status, axum::Json(envelope)).into_response();
        }
    }
    let envelope = json!({
        "error": {
            "code": "ERR.CHARTS.DATA_FETCHING_ERROR",
            "debug": {"message": message},
            "details": {"sources": {}},
            "statusCode": 427,
        }
    });
    (
        StatusCode::from_u16(427).unwrap_or(status),
        axum::Json(envelope),
    )
        .into_response()
}

fn oversize_error(code: &str, details: Value) -> (StatusCode, String) {
    let envelope = json!({"error": {"code": code, "details": details, "statusCode": 422}});
    (
        StatusCode::from_u16(422).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY),
        serde_json::to_string(&envelope).unwrap_or_default(),
    )
}

fn rows_oversize(limit: usize, current: usize) -> (StatusCode, String) {
    oversize_error(
        "ERR.CHARTS.ROWS_NUMBER_OVERSIZE",
        json!({"rowsLength": current, "rowsLimit": limit}),
    )
}

fn segments_oversize(limit: usize, current: usize) -> (StatusCode, String) {
    oversize_error(
        "ERR.CHARTS.SEGMENTS_OVERSIZE",
        json!({"segmentsCount": current, "segmentsLimit": limit}),
    )
}

fn table_oversize(cells: bool, limit: usize, current: usize) -> (StatusCode, String) {
    let details = if cells {
        json!({"type": "cells", "cellsCount": current, "cellsLimit": limit})
    } else {
        json!({"type": "columns", "columnsCount": current, "columnsLimit": limit})
    };
    oversize_error("ERR.CHARTS.TABLE_OVERSIZE", details)
}

/// How many rows the visualization is allowed to receive (`js.ts:487-604`).
fn graph_rows_limit(visualization_id: &str) -> usize {
    match visualization_id {
        "pie" | "donut" => 1_000,
        "treemap" | "funnel" => 800,
        _ => 75_000,
    }
}

fn result_rows(body: &Value) -> usize {
    body.pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

/// Distinct values of the first segment field in the data-api answer (`getSegmentsList`).
fn segments_number(config: &Value, body: &Value) -> usize {
    let Some(segment) = config
        .get("segments")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    else {
        return 0;
    };
    let guid = segment.get("guid").and_then(Value::as_str).unwrap_or("");
    if guid.is_empty() {
        return 0;
    }
    let fields = body.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
    let legend = body
        .pointer("/result_data/0/rows/0/legend")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let legend_item_id = fields.iter().find_map(|field| {
        let id = field.get("id").and_then(Value::as_str).unwrap_or("");
        (id == guid)
            .then(|| field.get("legend_item_id").and_then(Value::as_i64))
            .flatten()
    });
    let Some(index) = legend_item_id.and_then(|id| {
        legend.iter().position(|value| value.as_i64() == Some(id))
    }) else {
        return 0;
    };
    let mut distinct = std::collections::HashSet::new();
    if let Some(rows) = body.pointer("/result_data/0/rows").and_then(Value::as_array) {
        for row in rows {
            if let Some(value) = row.get("data").and_then(Value::as_array).and_then(|data| data.get(index)) {
                if !value.is_null() {
                    distinct.insert(value.to_string());
                }
            }
        }
    }
    distinct.len()
}

fn object_size(value: &Value) -> u64 {
    serde_json::to_string(value)
        .map(|text| text.len() as u64)
        .unwrap_or(0)
}


/// Exact compact-JSON length of `{"fields": <fields>, "revision_id": <revision>}` — the
/// `/fields` body a control reports in `sources.fields.size`. Counted, not built: the old
/// code cloned the whole fields array through `json!` and then serialized it again.
fn fields_body_json_len(fields: &Value, revision: &Value) -> u64 {
    // `{"fields":` (10) + `,"revision_id":` (15) + `}` (1)
    26 + dl_api::json_len(fields) + dl_api::json_len(revision)
}

/// The dataset a control reads: `config.data.shared.source.datasetId` for sourceType=datset.
fn control_dataset_ids(raw_body: &Value) -> Vec<String> {
    raw_body
        .pointer("/config/data/shared")
        .and_then(|shared| shared.get("source"))
        .and_then(|source| source.get("datasetId"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(|id| vec![id.to_string()])
        .unwrap_or_default()
}

/// Byte length of the compact JSON encoding of `value`, without allocating it.
/// `source.size` only needs the length, so the `/result` payload does not need to be
/// rendered to a `String` first.
fn json_len(value: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    if serde_json::to_writer(&mut counter, value).is_ok() {
        counter.0
    } else {
        0
    }
}

/// Response body from an already-built envelope buffer.
///
/// Bodies above [CHUNK] are streamed in slices instead of one `Bytes`: the compression layer
/// and the client start receiving before the whole payload was handed over, and the peak copy
/// per body is bounded. Small bodies keep the zero-copy `Body::from(Bytes)` path.
fn response_body(buffer: bytes::BytesMut) -> axum::body::Body {
    const CHUNK: usize = 256 * 1024;
    let bytes = buffer.freeze();
    if bytes.len() <= CHUNK {
        return axum::body::Body::from(bytes);
    }
    let mut chunks: Vec<Result<bytes::Bytes, std::io::Error>> = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + CHUNK).min(bytes.len());
        chunks.push(Ok(bytes.slice(offset..end)));
        offset = end;
    }
    axum::body::Body::from_stream(tokio_stream::iter(chunks))
}

/// v5.2 stage histograms. Samples are bounded; this is deliberately lightweight so it can
/// stay in the release image and feed /debug/stages.
///
/// `VecDeque`, not `Vec`: the old `samples.remove(0)` memmoved a 4096-element buffer on every
/// recorded sample once the histogram was full (three times per chart request).
static STAGE_SAMPLES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<&'static str, std::collections::VecDeque<f64>>>,
> = std::sync::OnceLock::new();

pub fn record_stage(name: &'static str, millis: f64) {
    let map = STAGE_SAMPLES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(mut map) = map.lock() {
        let samples = map.entry(name).or_default();
        if samples.len() >= 4096 {
            samples.pop_front();
        }
        samples.push_back(millis);
    }
}

pub async fn stage_stats() -> axum::Json<Value> {
    let Some(map) = STAGE_SAMPLES.get() else { return axum::Json(json!({})); };
    let Ok(map) = map.lock() else { return axum::Json(json!({})); };
    let mut out = serde_json::Map::new();
    for (name, samples_ref) in map.iter() {
        let mut samples: Vec<f64> = samples_ref.iter().copied().collect();
        if samples.is_empty() { continue; }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = samples.len();
        let pick = |p: f64| samples[((n as f64 * p) as usize).min(n - 1)];
        out.insert(name.to_string(), json!({
            "n": n,
            "min": samples[0],
            "p50": pick(0.50),
            "p95": pick(0.95),
            "max": samples[n-1],
            "mean": samples.iter().sum::<f64>() / n as f64,
        }));
    }
    axum::Json(Value::Object(out))
}

fn config_type_from_entry(entry_type: &str) -> String {
    match entry_type {
        "graph_wizard_node" => "graph_wizard_node".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Flat-table engine (V8 parity).
//
// Mirrors ui-dist `build-request-body` + `preparers/flat-table`:
//   * placeholders -> data-api request (roles, tree blocks, totals block,
//     shared filters + params filters, order_by, pagination, updates);
//   * data-api result -> `data` {head, rows, footer} with number formatting,
//     bar cells, tree cells, drill-down values and continuous background CSS;
//   * `config` exactly like buildChartsConfigPrivate (+ drillDown override and
//     the prepare-stage paginator override).
//
// Pivot tables use `run_rust_pivot`/`pivot_engine`; `run_rust_table` only
// handles the `flatTable` visualization.
// ---------------------------------------------------------------------------

const TABLE_MINIMUM_FRACTION_DIGITS: i64 = 2;
const TABLE_UNITS: [&str; 5] = ["", "K", "M", "B", "T"];
const TABLE_TOTALS_BG: &str = "var(--g-color-base-generic)";
const TABLE_DATE_FORMAT: &str = "DD.MM.YYYY";
const TABLE_DATETIME_FORMAT: &str = "DD.MM.YYYY HH:mm:ss";
const TABLE_DATETIMETZ_FORMAT: &str = "DD.MM.YYYY HH:mm:ss Z";

/// `ui-dist/shared/constants/gradients` (three-point + two-point).
/// Static gradient stop tables: these were rebuilt as a `Vec` on every table cell.
fn table_gradient(id: &str) -> Option<&'static [&'static str]> {
    Some(match id {
        "blue-gray-red" => &["#0044A3", "#D8D8D8", "#BA2534"],
        "blue-yellow-red" => &["#1B6DD1", "#FAAF1A", "#F23630"],
        "orange-blue-green" => &["#FC7E6A", "#8CCBFF", "#A5ED8E"],
        "orange-gray-blue" => &["#FF9580", "#D8D8D8", "#8CCBFF"],
        "orange-violet-blue" => &["#FC7E6A", "#6B32C9", "#8CCBFF"],
        "pink-gray-green" => &["#D0A3FF", "#D8D8D8", "#8EEDCB"],
        "red-orange-green" => &["#FF3D64", "#FFC636", "#54A520"],
        "traffic-light" => &["#9C0125", "#FDCE34", "#228782"],
        "violet-blue-green" => &["#6B32C9", "#57ABFF", "#79E0BF"],
        "yellow-green-blue" => &["#FFC533", "#64D1B4", "#69B6FF"],
        "red" => &["#BA2534", "#FF9580"],
        "green" => &["#0E752E", "#A5ED8E"],
        "blue" => &["#0044A3", "#8CCBFF"],
        "violet" => &["#6B32C9", "#D0A3FF"],
        "yellow" => &["#944710", "#FFE366"],
        "cyan" => &["#1C666E", "#C2EEBA"],
        "gray" => &["#EAEAEA", "#595959"],
        "golden" => &["#880132", "#FDEC4D"],
        "oceanic" => &["#050189", "#AEF8FE"],
        "violet-orange" => &["#6B32C9", "#FF9580"],
        "orange-yellow" => &["#FA5146", "#FFE366"],
        "red-blue" => &["#BA2534", "#1B6DD1"],
        "green-blue" => &["#A5ED8E", "#0044A3"],
        _ => return None,
    })
}

const TABLE_CLASSIC20: [&str; 20] = [
    "#4DA2F1", "#FF3D64", "#8AD554", "#FFC636", "#FFB9DD", "#84D1EE", "#FF91A1", "#54A520",
    "#DB9100", "#BA74B3", "#1F68A9", "#ED65A9", "#0FA08D", "#FF7E00", "#E8B0A4", "#52A6C5",
    "#BE2443", "#70C1AF", "#FFB46C", "#DCA3D7",
];

const TABLE_DEFAULT20: [&str; 20] = [
    "#3D97F2", "#FF6A59", "#7FD169", "#FFB433", "#AB94D9", "#8CD5FF", "#FF8CB2", "#60A196",
    "#4C73BF", "#B8C9CC", "#52CCC2", "#C3E55C", "#807D73", "#FF8C00", "#FFDC73", "#CC9966",
    "#CC5252", "#FFB28C", "#9661FF", "#D4BFFF",
];

/// `selectServerPalette` for the legacy names used by background/palette settings.
/// Static tables: the scheme was previously copied into a fresh `Vec<String>` per cell.
fn table_palette_scheme(palette: &str) -> Option<&'static [&'static str]> {
    match palette {
        "default-palette" | "classic20" => Some(&TABLE_CLASSIC20),
        "default20" => Some(&TABLE_DEFAULT20),
        _ => None,
    }
}

fn table_hex_to_rgb(hex: &str) -> (f64, f64, f64) {
    let channel = |range: std::ops::Range<usize>| -> f64 {
        hex.get(range)
            .and_then(|slice| u8::from_str_radix(slice, 16).ok())
            .unwrap_or(0) as f64
    };
    (channel(1..3), channel(3..5), channel(5..7))
}

/// Stand-in for an absent config object: reading `None`-ish members of this `Null`
/// returns the same `None` the old `cloned().unwrap_or_else(json!({}))` fallback did.
static MISSING_CONFIG: Value = Value::Null;

/// `colorizeByColorValues` for a continuous background gradient
/// (`utils/color-helpers.js`): linear RGB between the stops, `Math.round` per channel,
/// auto or manual thresholds, 2-point and 3-point palettes.
fn table_background_colors(
    values: &[Option<f64>],
    background: &Value,
) -> Vec<Option<String>> {
    let settings = background.get("settings").unwrap_or(&MISSING_CONFIG);
    let gradient_state = settings.get("gradientState").unwrap_or(&MISSING_CONFIG);
    let Some(palette) = gradient_state
        .get("gradientPalette")
        .and_then(Value::as_str)
        .and_then(table_gradient)
    else {
        return vec![None; values.len()];
    };
    let reversed = gradient_state
        .get("reversed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let stops: Vec<(f64, f64, f64)> = if reversed {
        palette.iter().rev().map(|hex| table_hex_to_rgb(hex)).collect()
    } else {
        palette.iter().map(|hex| table_hex_to_rgb(hex)).collect()
    };
    let present: Vec<f64> = values.iter().filter_map(|value| *value).collect();
    if present.is_empty() || stops.len() < 2 {
        return vec![None; values.len()];
    }
    let auto_min = present.iter().copied().fold(f64::INFINITY, f64::min);
    let auto_max = present.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let manual = gradient_state.get("thresholdsMode").and_then(Value::as_str) == Some("manual");
    // thresholds are stored as strings in the shared config ("-1", "0", "1")
    let number = |key: &str| {
        gradient_state
            .get(key)
            .and_then(table_parse_number)
            .filter(|value| value.is_finite())
    };
    let min = if manual { number("leftThreshold").unwrap_or(auto_min) } else { auto_min };
    let max = if manual { number("rightThreshold").unwrap_or(auto_max) } else { auto_max };
    let range = max - min;
    let three_point = gradient_state.get("gradientMode").and_then(Value::as_str) != Some("2-point");
    let mid = if three_point {
        if manual {
            number("middleThreshold").unwrap_or(max - range / 2.0)
        } else {
            max - range / 2.0
        }
    } else {
        max - range / 2.0
    };
    let round = |value: f64| -> i64 { (value + 0.5).floor() as i64 };
    let interpolate = |delta: f64| -> String {
        if three_point && delta >= 0.5 {
            let (left, right) = (stops[1], stops[2]);
            let local = (delta - 0.5) * 2.0;
            format!(
                "rgb({}, {}, {})",
                round((right.0 - left.0) * local + left.0),
                round((right.1 - left.1) * local + left.1),
                round((right.2 - left.2) * local + left.2)
            )
        } else {
            let (left, right) = (stops[0], stops[1]);
            let local = if three_point { delta * 2.0 } else { delta };
            format!(
                "rgb({}, {}, {})",
                round((right.0 - left.0) * local + left.0),
                round((right.1 - left.1) * local + left.1),
                round((right.2 - left.2) * local + left.2)
            )
        }
    };
    values
        .iter()
        .map(|value| {
            let value = (*value)?;
            let delta = if min == max {
                1.0
            } else if value <= min {
                0.0
            } else if value >= max {
                1.0
            } else if three_point && value >= mid {
                ((value - (2.0 * mid - max)) / (2.0 * (max - mid))).clamp(0.0, 1.0)
            } else if three_point {
                ((value - min) / (2.0 * (mid - min))).clamp(0.0, 1.0)
            } else {
                ((value - min) / range).clamp(0.0, 1.0)
            };
            Some(interpolate(delta))
        })
        .collect()
}

/// `getDiscreteBackgroundColorStyle`: mount a colour value to a palette index (or a literal
/// colour) from `paletteState.mountedColors`.
fn table_discrete_background_color(background: &Value, value: &Value) -> Option<String> {
    let settings = background.get("settings")?;
    let palette_state = settings.get("paletteState")?;
    let mounted = palette_state.get("mountedColors")?.as_object()?;
    let key = table_string(value);
    let mounted_value = mounted.get(&key)?;
    match mounted_value {
        Value::String(text) if text.starts_with('#') => Some(text.clone()),
        other => {
            let index = table_parse_number(other)? as i64;
            let palette = palette_state
                .get("palette")
                .and_then(Value::as_str)
                .unwrap_or("default-palette");
            let scheme = table_palette_scheme(palette)?;
            if scheme.is_empty() {
                return None;
            }
            Some(scheme[index.rem_euclid(scheme.len() as i64) as usize].to_string())
        }
    }
}

fn table_parse_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn table_is_measure(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("MEASURE") | Some("measure")
    )
}

fn table_is_parameter(item: &Value) -> bool {
    item.get("calc_mode").and_then(Value::as_str) == Some("parameter")
}

fn table_is_dimension(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("DIMENSION") | Some("dimension")
    ) && !table_is_parameter(item)
}

fn table_data_type(item: &Value) -> &str {
    item.get("data_type").and_then(|value| value.as_str()).unwrap_or("")
}

fn table_is_number_type(data_type: &str) -> bool {
    matches!(data_type, "float" | "integer" | "uinteger")
}

fn table_is_tree_type(data_type: &str) -> bool {
    matches!(data_type, "tree_str" | "tree_int" | "tree_float")
}

fn table_is_date_type(data_type: &str) -> bool {
    matches!(
        data_type,
        "date" | "datetime" | "genericdatetime" | "datetimetz"
    )
}

fn table_is_markup_type(data_type: &str) -> bool {
    data_type == "markup"
}

fn table_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn table_param_array<'a>(params: &'a Value, key: &str) -> Vec<&'a Value> {
    match params.get(key) {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(value) => vec![value],
        None => Vec::new(),
    }
}

fn table_param_u64(params: &Value, key: &str) -> Option<u64> {
    table_param_array(params, key)
        .first()
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        })
}

/// `isRawParamValid` + `isParamValid` from the request builder.
fn table_param_filter_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().map(|number| !number.is_nan()).unwrap_or(false),
        Value::Array(items) => {
            if items.is_empty() || items.iter().any(Value::is_null) {
                return false;
            }
            !(items.len() == 1 && items.first().and_then(Value::as_str) == Some(""))
        }
        _ => true,
    }
}

#[derive(Clone)]
struct TableDrill {
    breadcrumbs: Vec<String>,
    guids: Vec<String>,
    data_types: Vec<String>,
    level: usize,
}

impl TableDrill {
    fn active_guid(&self) -> &str {
        self.guids.get(self.level).map(String::as_str).unwrap_or("")
    }

    fn date_format(&self) -> &'static str {
        let data_type = self
            .data_types
            .get(self.level)
            .map(String::as_str)
            .unwrap_or("");
        if matches!(data_type, "datetime" | "genericdatetime") {
            "YYYY-MM-DDTHH:mm:ss"
        } else {
            "YYYY-MM-DD"
        }
    }
}

/// `preprocessHierarchies`: replaces the hierarchy pseudo-field with the field
/// of the current drill-down level and keeps the breadcrumb metadata.
fn table_expand_hierarchies(shared: &Value, params: &Value) -> (Value, Option<TableDrill>) {
    let mut expanded = shared.clone();
    let level = table_param_u64(params, "drillDownLevel").unwrap_or(0) as usize;
    let mut drill = None;
    if let Some(placeholders) = expanded
        .pointer_mut("/visualization/placeholders")
        .and_then(Value::as_array_mut)
    {
        'outer: for placeholder in placeholders.iter_mut() {
            let Some(items) = placeholder.get_mut("items").and_then(Value::as_array_mut) else {
                continue;
            };
            for index in 0..items.len() {
                let is_hierarchy = items[index].get("data_type").and_then(Value::as_str)
                    == Some("hierarchy");
                let Some(fields) = items[index].get("fields").and_then(Value::as_array).cloned()
                else {
                    continue;
                };
                if !is_hierarchy || fields.is_empty() {
                    continue;
                }
                let current = level.min(fields.len() - 1);
                items[index] = fields[current].clone();
                let breadcrumbs = fields
                    .iter()
                    .map(|field| {
                        field
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect();
                let guids = fields
                    .iter()
                    .map(|field| {
                        field
                            .get("guid")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect();
                let data_types = fields
                    .iter()
                    .map(|field| {
                        field
                            .get("data_type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect();
                drill = Some(TableDrill {
                    breadcrumbs,
                    guids,
                    data_types,
                    level: current,
                });
                break 'outer;
            }
        }
    }
    (expanded, drill)
}

fn table_columns(shared: &Value) -> Vec<&Value> {
    shared
        .pointer("/visualization/placeholders/0/items")
        .and_then(Value::as_array)
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

fn table_placeholder_items(shared: &Value) -> Vec<&Value> {
    let mut items = Vec::new();
    if let Some(placeholders) = shared
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)
    {
        for placeholder in placeholders {
            if let Some(placeholder_items) = placeholder.get("items").and_then(Value::as_array) {
                items.extend(placeholder_items.iter());
            }
        }
    }
    items
}

/// Parsed `entry.data.shared` config cache.
///
/// US stores `shared` as a JSON string (32-104 KB in real dashboards). The entry cache
/// already revision-validates the entry, so parsing this string again on every chart request
/// is avoidable work: cache the parsed `Value` by (branch, entry, revId). The revId in the
/// key makes a revision change a cache miss; no stale config is served. Access control is
/// enforced by the entry read that produced this text, and the shared block is chart config,
/// not row data.
fn parsed_shared_cached(
    entry_id: &str,
    branch: &str,
    revision: &str,
    text: &str,
) -> Option<std::sync::Arc<Value>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static MAX: OnceLock<usize> = OnceLock::new();
    static TTL: OnceLock<Duration> = OnceLock::new();
    /// `PARSED_SHARED_CACHE_MAX` (default 512) / `PARSED_SHARED_CACHE_TTL_SECS` (default 0 =
    /// revision-keyed only). The revision in the key still guarantees freshness.
    let max = *MAX.get_or_init(|| {
        std::env::var("PARSED_SHARED_CACHE_MAX")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(512)
    });
    let ttl = *TTL.get_or_init(|| {
        Duration::from_secs(
            std::env::var("PARSED_SHARED_CACHE_TTL_SECS")
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0),
        )
    });
    let parse = || serde_json::from_str::<Value>(text).ok().map(Arc::new);
    if max == 0 || revision.is_empty() {
        return parse();
    }
    let key = (branch.to_string(), entry_id.to_string(), revision.to_string());
    let cache = PARSED_SHARED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut cache) = cache.lock() {
        if let Some((stamp, value)) = cache.get(&key) {
            if ttl.is_zero() || stamp.elapsed() < ttl {
                return Some(Arc::clone(value));
            }
            cache.remove(&key);
        }
    }
    let value = parse()?;
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= max && !cache.contains_key(&key) {
            if let Some(oldest) = cache.keys().next().cloned() {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, (Instant::now(), Arc::clone(&value)));
    }
    Some(value)
}

/// `getFormatOptions` from ui-dist/shared/modules/fields.js.
fn table_format_options(item: &Value) -> Value {
    let mut options = item
        .get("formatting")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if options
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(true)
    {
        if let Some(number_formatting) = item
            .get("ui_settings")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .and_then(|settings| settings.get("numberFormatting").cloned())
        {
            options = number_formatting;
        }
    }
    if options.get("precision").and_then(Value::as_f64).is_none()
        && table_data_type(item) == "float"
    {
        if let Some(object) = options.as_object_mut() {
            object.insert(
                "precision".to_string(),
                json!(TABLE_MINIMUM_FRACTION_DIGITS),
            );
        }
    }
    options
}

fn table_unit_rate(value: f64) -> i64 {
    let mut rate = 1i64;
    while rate < 10
        && TABLE_UNITS.get(rate as usize).is_some()
        && (value / 1000f64.powi(rate as i32)).abs() >= 1.0
    {
        rate += 1;
    }
    rate - 1
}

fn table_group_digits(text: &str) -> String {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    // Group sizes come from a counter instead of `(len - index) % 3`: the digits run is
    // short, so shaving a division per character is a cheap win on the formatting path.
    let mut first_group = digits.len();
    while first_group > 3 {
        first_group -= 3;
    }
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    let mut left = first_group;
    for character in digits.chars() {
        if left == 0 {
            grouped.push('\u{a0}');
            left = 3;
        }
        grouped.push(character);
        left -= 1;
    }
    format!("{sign}{grouped}")
}

/// `Intl.NumberFormat("ru-RU", { minimumFractionDigits, maximumFractionDigits: 20 })`.
fn table_intl_number(value: f64, minimum_fraction_digits: i64, grouping: bool) -> String {
    let text = if minimum_fraction_digits > 0 {
        crate::js_format::fixed_half_away(value, minimum_fraction_digits as usize)
    } else {
        format!("{value}")
    };
    let (whole, mut fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole.to_string(), fraction.to_string()),
        None => (text.clone(), String::new()),
    };
    while (fraction.len() as i64) < minimum_fraction_digits {
        fraction.push('0');
    }
    let whole = if grouping {
        table_group_digits(&whole)
    } else {
        whole
    };
    if fraction.is_empty() {
        whole
    } else {
        format!("{whole},{fraction}")
    }
}

fn table_js_to_fixed(value: f64, digits: i64) -> f64 {
    crate::js_format::fixed_half_away(value, digits.max(0) as usize)
        .parse()
        .unwrap_or(value)
}

/// `formatNumber` from ui-dist/shared/modules/format-units.
fn table_format_number(value: f64, options: &Value) -> String {
    if value.is_nan() {
        return format!("{value}");
    }
    let format = options
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("number");
    let multiplier = options
        .get("multiplier")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let prefix = options.get("prefix").and_then(|value| value.as_str()).unwrap_or("");
    let postfix = options
        .get("postfix")
        .and_then(Value::as_str)
        .unwrap_or("");
    let label_mode = options.get("labelMode").and_then(Value::as_str);
    let mut changed_multiplier = multiplier;
    let mut pre_postfix = "";
    if format == "percent" {
        changed_multiplier = 100.0;
        pre_postfix = "%";
    }
    if label_mode == Some("percent") {
        pre_postfix = "%";
    }
    let explicit_rate = options.get("unitRate").and_then(Value::as_f64);
    let unit = options.get("unit").and_then(Value::as_str);
    let unit_rate = match explicit_rate {
        Some(rate) => Some(rate as i64),
        None => match unit {
            Some("auto") => None,
            Some("k") => Some(1),
            Some("m") => Some(2),
            Some("b") => Some(3),
            Some("t") => Some(4),
            _ => Some(0),
        },
    };
    let base = value * changed_multiplier;
    let rate = unit_rate.unwrap_or_else(|| table_unit_rate(base));
    let mut result = base / 1000f64.powi(rate as i32);
    let mut minimum_fraction_digits = 0i64;
    match options.get("precision") {
        Some(Value::Number(number)) => {
            if let Some(digits) = number.as_f64() {
                minimum_fraction_digits = digits as i64;
                result = table_js_to_fixed(result, minimum_fraction_digits);
            }
        }
        Some(Value::String(text)) if text == "auto"
            && result.fract() != 0.0 => {
                let digits = if result.abs() > 1.0 { 2 } else { 4 };
                result = table_js_to_fixed(result, digits);
            }
        _ => {}
    }
    let grouping = options
        .get("showRankDelimiter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let formatted = table_intl_number(result, minimum_fraction_digits, grouping);
    let unit_text = TABLE_UNITS.get(rate as usize).copied().unwrap_or("");
    format!("{prefix}{formatted}{pre_postfix}{unit_text}{postfix}")
}

fn table_column_width(item: &Value) -> Option<String> {
    let width = item.pointer("/columnSettings/width")?;
    match width.get("mode").and_then(Value::as_str) {
        Some("percent") => Some(format!(
            "{}%",
            width.get("value").and_then(|value| value.as_str()).unwrap_or("")
        )),
        Some("pixel") => Some(format!(
            "{}px",
            width.get("value").and_then(|value| value.as_str()).unwrap_or("")
        )),
        _ => None,
    }
}

/// `getBarSettingsViewOptions`.
fn table_bars_view_options(
    item: &Value,
    column_values: &[Value],
) -> serde_json::Map<String, Value> {
    let bars = item.get("barsSettings").cloned().unwrap_or_else(|| json!({}));
    let mut options = serde_json::Map::new();
    options.insert("view".to_string(), json!("bar"));
    options.insert(
        "showLabel".to_string(),
        json!(bars
            .get("showLabels")
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    let scale = bars.get("scale").cloned().unwrap_or_else(|| json!({}));
    let (min, max) = if scale.get("mode").and_then(Value::as_str) == Some("auto") {
        let numbers: Vec<f64> = column_values.iter().filter_map(table_parse_number).collect();
        (
            numbers.iter().copied().fold(f64::INFINITY, f64::min),
            numbers
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max),
        )
    } else {
        let settings = scale.get("settings").cloned().unwrap_or_else(|| json!({}));
        let number = |value: Option<&Value>| match value {
            None => None,
            Some(Value::Null) => Some(0.0),
            Some(other) => table_parse_number(other),
        };
        (
            number(settings.get("min")).unwrap_or(f64::NAN),
            number(settings.get("max")).unwrap_or(f64::NAN),
        )
    };
    let min_correct = !min.is_nan() && min <= 0.0;
    let max_correct = !max.is_nan() && max >= 0.0;
    if min_correct && max_correct && min == max {
        options.insert("max".to_string(), metric_json_number(min));
    } else {
        if min_correct {
            options.insert("min".to_string(), metric_json_number(min));
        }
        if max_correct {
            options.insert("max".to_string(), metric_json_number(max));
        }
    }
    if let Some(align) = bars.get("align").and_then(Value::as_str) {
        if align != "default" {
            options.insert("align".to_string(), json!(align));
        }
    }
    options
}

/// `getBarSettingsValue` (one-color / two-color; palette/index colors are not
/// used by the tables in this deployment and fall back to an empty color).
fn table_bar_cell(item: &Value, row_value: &Value) -> (Value, String, Value, bool) {
    let bars = item.get("barsSettings").cloned().unwrap_or_else(|| json!({}));
    let color_settings = bars
        .pointer("/colorSettings/settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let color_type = bars
        .pointer("/colorSettings/colorType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bar_color = match color_type {
        "one-color" => color_settings
            .get("color")
            .cloned()
            .unwrap_or(Value::Null),
        "two-color" => {
            let positive = table_parse_number(row_value)
                .map(|number| number >= 0.0)
                .unwrap_or(true);
            color_settings
                .get(if positive {
                    "positiveColor"
                } else {
                    "negativeColor"
                })
                .cloned()
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    let format_options = table_format_options(item);
    let (value, formatted_value) = match table_parse_number(row_value) {
        Some(number) => (
            metric_json_number(number),
            table_format_number(number, &format_options),
        ),
        None => (metric_json_number(0.0), table_string(row_value)),
    };
    let show_bar = bars
        .get("showBarsInTotals")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (value, formatted_value, bar_color, show_bar)
}

fn table_apply_formatter(
    cell: &mut serde_json::Map<String, Value>,
    options: &Value,
    data_type: &str,
) {
    let mut formatter = serde_json::Map::new();
    for (target, source) in [
        ("format", "format"),
        ("suffix", "postfix"),
        ("prefix", "prefix"),
        ("showRankDelimiter", "showRankDelimiter"),
        ("unit", "unit"),
    ] {
        if let Some(value) = options.get(source) {
            formatter.insert(target.to_string(), value.clone());
        }
    }
    // `prepareFlatTable`: `precision` is only added for float fields (JS checks the type).
    if data_type == "float" {
        formatter.insert(
            "precision".to_string(),
            options
                .get("precision")
                .cloned()
                .unwrap_or_else(|| json!(TABLE_MINIMUM_FRACTION_DIGITS)),
        );
    }
    cell.insert("formatter".to_string(), Value::Object(formatter));
}

fn table_legend_ids(row: &Value) -> Vec<i64> {
    row.get("legend")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn table_field_by_legend(fields: &[Value], legend_id: i64) -> Option<&Value> {
    fields
        .iter()
        .find(|field| field.get("legend_item_id").and_then(Value::as_i64) == Some(legend_id))
}

fn table_field_role(field: &Value) -> &str {
    field
        .pointer("/role_spec/role")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn table_index_by_guid(fields: &[Value], legend_ids: &[i64], guid: &str) -> Option<usize> {
    for (position, legend_id) in legend_ids.iter().enumerate() {
        let Some(field) = table_field_by_legend(fields, *legend_id) else {
            continue;
        };
        if matches!(
            table_field_role(field),
            "order_by" | "filter" | "info" | "total" | "template"
        ) {
            continue;
        }
        if field.get("id").and_then(Value::as_str) == Some(guid) {
            return Some(position);
        }
    }
    None
}

fn table_tree_state(params: &Value) -> Vec<String> {
    table_param_array(params, "treeState")
        .into_iter()
        .map(table_string)
        .filter(|value| !value.is_empty())
        .collect()
}

/// `json.dumps(value)`-compatible serialization (Python's default separators are `", "`
/// and `": "`), which is what the reference data-api writes into a tree cell's `treeNode`.
///
/// The UI matches `cell.treeNode` against the `treeState` entries it sends back on the next
/// drill-down request, and those entries are produced by the reference stack with Python's
/// spacing. `serde_json::to_string` writes compact JSON, so with it every tree node looked
/// unknown and the table never expanded (`treeNodeState` stayed `closed`).
fn python_json(value: &Value) -> String {
    struct PyFormatter;
    impl serde_json::ser::Formatter for PyFormatter {
        fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }
        fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }
        fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            writer.write_all(b": ")
        }
    }
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, PyFormatter);
    match value.serialize(&mut serializer) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Whether any of the client's `treeState` entries denotes exactly this tree path.
///
/// The strings are compared after parsing, so a difference in whitespace (`", "` vs `","`)
/// or key order can never leave a node permanently "closed" again; the raw string form is
/// kept as a fallback for malformed entries.
fn tree_state_contains_path(tree_state: &[String], parsed: &[Value]) -> bool {
    tree_state.iter().any(|entry| {
        match serde_json::from_str::<Value>(entry) {
            Ok(Value::Array(items)) => items == parsed,
            _ => entry == &python_json(&Value::Array(parsed.to_vec())),
        }
    })
}

/// `getPayloadWithCommonTableSettings` pagination part.
fn table_pagination(shared: &Value, params: &Value) -> (bool, Option<u64>) {
    let extra = shared.get("extraSettings").unwrap_or(&Value::Null);
    let items = table_placeholder_items(shared);
    let has_dimensions = items
        .iter()
        .any(|item| table_is_dimension(item) || table_data_type(item) == "hierarchy");
    let limit = extra.get("limit").and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    });
    let page_enabled =
        has_dimensions && extra.get("pagination").and_then(Value::as_str) == Some("on");
    let _ = params;
    (page_enabled && limit.is_some(), limit)
}

fn table_matched_params(shared: &Value, params: &Value) -> Vec<String> {
    let mut matched = Vec::new();
    let js_truthy = |value: &Value| match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number
            .as_f64()
            .map(|number| number != 0.0 && !number.is_nan())
            .unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        _ => true,
    };
    if let Some(datasets) = shared
        .get("datasetsPartialFields")
        .and_then(Value::as_array)
    {
        for fields in datasets {
            for field in fields.as_array().into_iter().flatten() {
                let Some(title) = field.get("title").and_then(Value::as_str) else {
                    continue;
                };
                if params
                    .get(title)
                    .map(&js_truthy)
                    .unwrap_or(false)
                {
                    matched.push(title.to_string());
                }
            }
        }
    }
    matched
}

/// data-api request body (`build-request-body` for flat tables, apiVersion 2).
fn find_schema_field<'a>(
    dataset_fields: &'a Value,
    shared: &'a Value,
    key: &str,
) -> Option<&'a Value> {
    let matches = |field: &Value| {
        field.get("guid").and_then(Value::as_str) == Some(key)
            || field.get("title").and_then(Value::as_str) == Some(key)
    };
    if let Some(fields) = dataset_fields.as_array() {
        if let Some(field) = fields.iter().find(|field| matches(field)) {
            return Some(field);
        }
    }
    shared
        .get("datasetsPartialFields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .find(|field| matches(field))
}

/// Cache the immutable data-api request body for repeated identical chart requests.
///
/// The key includes chart entry/revision, dataset revision and the serialized request params,
/// so any interaction/schema change is a miss. This is chart config/filter state, not row data.
fn table_request_cached(
    shared: &Value,
    params: &Value,
    dataset: &str,
    revision: &Value,
    dataset_fields: &Value,
    drill: Option<&TableDrill>,
    branch: &str,
    meta_entry_id: &Value,
    meta_rev: &Value,
    lang: &str,
) -> std::sync::Arc<Value> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static MAX: OnceLock<usize> = OnceLock::new();
    /// `TABLE_REQUEST_CACHE_MAX` (default 256) bounds this per-request-shape cache.
    let max = *MAX.get_or_init(|| {
        std::env::var("TABLE_REQUEST_CACHE_MAX")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(256)
    });
    // Diagnostic/profiling switch: `TABLE_REQUEST_CACHE=0` reproduces the pre-v233 path.
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| {
        std::env::var("TABLE_REQUEST_CACHE")
            .map(|value| value != "0")
            .unwrap_or(true)
    });
    if !enabled {
        return Arc::new(table_request(shared, params, dataset, revision, dataset_fields, drill));
    }
    let rev = meta_rev.as_str().unwrap_or("");
    let key = if rev.is_empty() {
        None
    } else {
        serde_json::to_string(params).ok().map(|params_text| {
            format!(
                "{branch}|{}|{}|{dataset}|{revision}|{lang}|{params_text}",
                meta_entry_id, rev
            )
        })
    };
    let Some(key) = key else {
        return Arc::new(table_request(shared, params, dataset, revision, dataset_fields, drill));
    };
    let cache = TABLE_REQUEST_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(value) = cache.get(&key) {
            return Arc::clone(value);
        }
    }
    let value = Arc::new(table_request(shared, params, dataset, revision, dataset_fields, drill));
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= max && !cache.contains_key(&key) {
            if let Some(oldest) = cache.keys().next().cloned() {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, Arc::clone(&value));
    }
    value
}

enum ExtraColumn<'a> {
    Borrowed(&'a Value),
    Owned(Value),
}

impl ExtraColumn<'_> {
    fn value(&self) -> &Value {
        match self {
            ExtraColumn::Borrowed(value) => value,
            ExtraColumn::Owned(value) => value,
        }
    }
}

fn table_push_extra<'a>(
    item: &'a Value,
    field_guids: &mut std::collections::HashSet<&'a str>,
    extra_columns: &mut Vec<ExtraColumn<'a>>,
) {
    if table_is_parameter(item) {
        return;
    }
    let Some(guid) = item.get("guid").and_then(Value::as_str) else {
        return;
    };
    if guid.is_empty() || !field_guids.insert(guid) {
        return;
    }
    extra_columns.push(ExtraColumn::Borrowed(item));
}

fn table_request(
    shared: &Value,
    params: &Value,
    dataset: &str,
    revision: &Value,
    dataset_fields: &Value,
    drill: Option<&TableDrill>,
) -> Value {
    let visualization = shared.get("visualization").unwrap_or(&Value::Null);
    let visualization_id = visualization
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let extra = shared.get("extraSettings").unwrap_or(&Value::Null);
    let items = table_placeholder_items(shared);
    let fields: Vec<&Value> = items
        .iter()
        .copied()
        .filter(|item| !table_is_parameter(item))
        .collect();
    let is_measure_in_fields = fields.iter().copied().any(table_is_measure);
    let with_totals = visualization_id == "flatTable"
        && extra.get("totals").and_then(Value::as_str) == Some("on")
        && is_measure_in_fields;
    let is_tree_request = fields
        .iter()
        .copied()
        .any(|item| table_is_tree_type(table_data_type(item)));
    let tree_state = table_tree_state(params);
    let mut request_fields: Vec<Value> = Vec::new();
    for item in &fields {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        if table_is_tree_type(table_data_type(item)) {
            request_fields.push(json!({
                "ref": {"type": "id", "id": guid},
                "legend_item_id": 1,
                "role_spec": {"prefix": "[]", "dimension_values": [], "role": "tree", "level": 1},
                "block_id": 0,
            }));
            // `getTreeRequestFields` (default-request.ts): `initialLegendItemId` is 1, the
            // first tree-state entry gets id 3 (the loop pre-increments from 2), and
            // `legendDict` is keyed by compact `JSON.stringify` so parent lookups are
            // independent of the client's whitespace.
            let mut legend: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            legend.insert("[]".to_string(), 1);
            let mut legend_item_id = 2i64;
            for (index, prefix) in tree_state.iter().enumerate() {
                let parsed: Vec<Value> = serde_json::from_str(prefix).unwrap_or_default();
                let key = serde_json::to_string(&parsed).unwrap_or_default();
                let previous_key =
                    serde_json::to_string(&parsed[..parsed.len().saturating_sub(1)])
                        .unwrap_or_default();
                legend_item_id += 1;
                legend.insert(key, legend_item_id);
                // The reference forwards the client's `prefix` string verbatim; the tree
                // data-api compares that string with its own tree values, which are rendered
                // by Python `json.dumps` (", " separators). A compact client string (which is
                // what our own UI used to send) therefore matched nothing and the deeper
                // blocks came back empty — normalise to the Python-style text here, and keep
                // the client formatting out of the wire contract.
                let prefix_text = python_json(&Value::Array(parsed.clone()));
                let dimension_values = if parsed.is_empty() {
                    json!([])
                } else {
                    json!([{
                        "legend_item_id": legend.get(&previous_key).copied().unwrap_or(1),
                        "value": prefix_text,
                    }])
                };
                request_fields.push(json!({
                    "ref": {"type": "id", "id": guid},
                    "legend_item_id": legend_item_id,
                    "role_spec": {
                        "prefix": prefix_text,
                        "dimension_values": dimension_values,
                        "role": "tree",
                        "level": parsed.len() + 1,
                    },
                    "block_id": 1 + index as i64,
                }));
            }
            continue;
        }
        let mut field = serde_json::Map::new();
        field.insert("ref".to_string(), json!({"type": "id", "id": guid}));
        // `buildDefaultRequest` order: ref, block_id, role_spec.
        if !is_tree_request {
            field.insert("block_id".to_string(), json!(0));
        }
        if table_is_measure(item) {
            field.insert("role_spec".to_string(), json!({"role": "measure"}));
        }
        request_fields.push(Value::Object(field));
    }
    // `payload.columns`: placeholder fields first, then colours/labels/tooltips/shapes/
    // segments and finally the sort fields (`prepareSingleRequest`).
    let mut extra_columns: Vec<ExtraColumn> = Vec::new();
    let mut field_guids: std::collections::HashSet<&str> = fields
        .iter()
        .filter_map(|item| item.get("guid").and_then(Value::as_str))
        .collect();
    // `prepareColumns`: continuous background-settings colour fields are added to the request
    // columns (flat tables only use the continuous variant).
    for item in &fields {
        if item
            .pointer("/backgroundSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && item
                .pointer("/backgroundSettings/settings/isContinuous")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            if let Some(color_field) = item
                .pointer("/backgroundSettings/colorFieldGuid")
                .and_then(Value::as_str)
            {
                if field_guids.insert(color_field) {
                    extra_columns.push(ExtraColumn::Owned(json!({"guid": color_field})));
                }
            }
        }
    }
    for key in ["colors", "labels", "tooltips", "shapes", "segments"] {
        for item in shared.get(key).and_then(Value::as_array).into_iter().flatten() {
            table_push_extra(item, &mut field_guids, &mut extra_columns);
        }
    }
    for item in shared.get("sort").and_then(Value::as_array).into_iter().flatten() {
        table_push_extra(item, &mut field_guids, &mut extra_columns);
    }
    for column in &extra_columns {
        let item = column.value();
        let mut field = serde_json::Map::new();
        field.insert("ref".to_string(), json!({"type": "id", "id": item.get("guid").cloned().unwrap_or(Value::Null)}));
        if !is_tree_request {
            field.insert("block_id".to_string(), json!(0));
        }
        if table_is_measure(item) {
            field.insert("role_spec".to_string(), json!({"role": "measure"}));
        }
        request_fields.push(Value::Object(field));
    }
    if with_totals {
        for item in &fields {
            if table_is_dimension(item) {
                request_fields.push(json!({
                    "ref": {"type": "placeholder"},
                    "role_spec": {"role": "template", "template": ""},
                    "block_id": 1,
                }));
            } else if table_is_measure(item) {
                request_fields.push(json!({
                    "ref": {"type": "id", "id": item.get("guid").cloned().unwrap_or(Value::Null)},
                    "role_spec": {"role": "total"},
                    "block_id": 1,
                }));
            }
        }
        // `buildTotalRequestFields`: extra payload columns not present in `fields` become
        // template placeholders too (e.g. a sort-only dimension).
        for column in &extra_columns {
            let _item = column.value();
            request_fields.push(json!({
                "ref": {"type": "placeholder"},
                "role_spec": {"role": "template", "template": ""},
                "block_id": 1,
            }));
        }
    }
    // Filters: chart filters first, then params filters.
    let mut chart_filters: Vec<(String, String, Vec<Value>)> = Vec::new();
    if let Some(filters) = shared.get("filters").and_then(Value::as_array) {
        for filter in filters {
            if filter
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(filter_dataset) = filter.get("datasetId").and_then(Value::as_str) {
                if filter_dataset != dataset {
                    continue;
                }
            }
            let Some(definition) = filter.get("filter") else {
                continue;
            };
            let operation = definition
                .pointer("/operation/code")
                .and_then(Value::as_str)
                .or_else(|| definition.get("operation").and_then(Value::as_str))
                .unwrap_or("IN")
                .to_string();
            let Some(value) = definition.get("value") else {
                continue;
            };
            let values = match value {
                Value::Array(items) => items.clone(),
                Value::Null => continue,
                Value::String(text) if text.is_empty() => continue,
                other => vec![other.clone()],
            };
            // A dashboard date range is saved as one `__interval_*` string (its sides may be
            // `__relative_*`); the data-api needs the resolved pair for `BETWEEN`.
            let (values, operation) = crate::pivot_engine::resolve_filter_values(&values, &operation);
            if values.is_empty() {
                continue;
            }
            chart_filters.push((
                filter
                    .get("guid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                operation,
                values,
            ));
        }
    }
    // `splitParamsToParametersAndFilters`: parameter fields go to `parameter_values`
    // (`{ref, value}`), everything else becomes a filter.
    let mut parameter_values: Vec<Value> = Vec::new();
    let mut parameter_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(values) = params.as_object() {
        for (key, value) in values {
            if key == "__datasetId" || !table_param_filter_valid(value) {
                continue;
            }
            let found = find_schema_field(dataset_fields, shared, key);
            let is_parameter = found
                .map(|field| field.get("calc_mode").and_then(Value::as_str) == Some("parameter"))
                .unwrap_or(false);
            if !is_parameter {
                continue;
            }
            let raw_values: Vec<Value> = match value {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            let parameter_value = if raw_values.len() == 1 {
                raw_values[0].clone()
            } else {
                Value::Array(raw_values)
            };
            parameter_values.push(json!({
                "ref": {"type": "id", "id": key},
                "value": parameter_value,
            }));
            parameter_keys.insert(key.clone());
        }
    }
    let mut params_filters: Vec<(String, String, Vec<Value>)> = Vec::new();
    if let Some(values) = params.as_object() {
        for (key, value) in values {
            if key == "__datasetId" || parameter_keys.contains(key) || !table_param_filter_valid(value) {
                continue;
            }
            let raw_values: Vec<Value> = match value {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            let found_field = find_schema_field(dataset_fields, shared, key);
            let is_date = found_field
                .map(|field| table_is_date_type(table_data_type(field)))
                .unwrap_or(false);
            let default_operation = if is_date && raw_values.len() == 1 {
                "EQ"
            } else {
                "IN"
            };
            let mut joined: std::collections::HashMap<String, Vec<Value>> =
                std::collections::HashMap::new();
            let mut separate: Vec<(String, String, Vec<Value>)> = Vec::new();
            for raw in raw_values {
                // `__interval_*` / `__relative_*` markers (a dashboard date range arrives
                // as a single string) are resolved like the JS engine resolves them.
                let (raw_operation, raw_values_resolved) = if let Value::String(_) = &raw {
                    let (resolved, resolved_operation) = crate::pivot_engine::resolve_filter_values(
                        std::slice::from_ref(&raw),
                        default_operation,
                    );
                    if resolved.is_empty() {
                        continue;
                    }
                    (resolved_operation, resolved)
                } else {
                    if raw.is_null() {
                        continue;
                    }
                    (default_operation.to_string(), vec![raw])
                };
                if raw_operation == "IN" || raw_operation == "NIN" {
                    joined
                        .entry(raw_operation)
                        .or_default()
                        .extend(raw_values_resolved);
                } else {
                    separate.push((key.clone(), raw_operation, raw_values_resolved));
                }
            }
            for operation in ["IN", "NIN"] {
                if let Some(grouped) = joined.remove(operation) {
                    if !grouped.is_empty() {
                        params_filters.push((key.clone(), operation.to_string(), grouped));
                    }
                }
            }
            params_filters.extend(separate);
        }
    }
    let params_columns: std::collections::HashSet<String> = params_filters
        .iter()
        .map(|(column, _, _)| column.clone())
        .collect();
    let mut filters: Vec<Value> = chart_filters
        .into_iter()
        .filter(|(column, _, _)| !params_columns.contains(column))
        .map(|(column, operation, values)| {
            json!({"ref": {"type": "id", "id": column}, "operation": operation, "values": values})
        })
        .collect();
    filters.extend(
        params_filters
            .into_iter()
            .map(|(column, operation, values)| {
                json!({"ref": {"type": "id", "id": column}, "operation": operation, "values": values})
            }),
    );
    // `buildRequestFilters` in the reference engine appends one filter per applied drill-down
    // level: `drillDownFilters[0..level]` are the values the user already drilled through, and
    // the field of each level is the corresponding hierarchy field. Dropping them returned the
    // unfiltered level instead: page 2 of a drilled table showed 100 unrelated rows (and the
    // UI looked like it "did not expand"). Empty values are not filters (the root level).
    if let Some(drill) = drill {
        let drill_values = table_param_array(params, "drillDownFilters");
        for index in 0..drill.level.min(drill.guids.len()).min(drill_values.len()) {
            let value = table_string(drill_values[index]);
            if value.is_empty() {
                continue;
            }
            filters.push(json!({
                "ref": {"type": "id", "id": drill.guids[index]},
                "operation": "IN",
                "values": [value],
            }));
        }
    }
    // order_by: shared.sort (default DESC) + dimensions ASC when the table paginates.
    let mut order_by: Vec<(String, String)> = Vec::new();
    if let Some(sort) = shared.get("sort").and_then(Value::as_array) {
        for item in sort {
            if let Some(item_dataset) = item.get("datasetId").and_then(Value::as_str) {
                if item_dataset != dataset {
                    continue;
                }
            }
            let Some(guid) = item.get("guid").and_then(Value::as_str) else {
                continue;
            };
            let direction = item
                .get("direction")
                .and_then(Value::as_str)
                .unwrap_or("DESC")
                .to_lowercase();
            order_by.push((guid.to_string(), direction));
        }
    }
    let (pagination_on, limit) = table_pagination(shared, params);
    if visualization_id == "flatTable" && pagination_on {
        // `getOrderByItemForColumnClickSort` for a flat table: `_columnId` is
        // `_id=<field guid>_name=<column name>`, and the clicked column is put in front of
        // `shared.sort` (`getUpdatedOrderByForColumnClickSort`).
        let clicked = table_param_array(params, "_columnId")
            .first()
            .map(|value| table_string(value))
            .filter(|column_id| !column_id.is_empty())
            .and_then(|column_id| {
                let rest = column_id.split("_id=").nth(1)?;
                let field_part = rest.split("_name=").next()?;
                Some(field_part.to_string())
            });
        if let Some(guid) = clicked {
            if fields.iter().any(|item| {
                item.get("guid").and_then(Value::as_str) == Some(guid.as_str())
            }) {
                let sort_order = table_param_u64(params, "_sortOrder").unwrap_or(0);
                let direction = if sort_order == 1 { "asc" } else { "desc" };
                order_by.insert(0, (guid, direction.to_string()));
            }
        }
        for item in &fields {
            if table_is_dimension(item) {
                if let Some(guid) = item.get("guid").and_then(Value::as_str) {
                    if !order_by.iter().any(|(column, _)| column == guid) {
                        order_by.push((guid.to_string(), "asc".to_string()));
                    }
                }
            }
        }
    }
    // lodash `unionBy(order_by, dimensions, 'column')` keeps the first occurrence per column.
    let mut seen_order_columns: Vec<String> = Vec::new();
    order_by.retain(|(column, _)| {
        if seen_order_columns.iter().any(|known| known == column) {
            false
        } else {
            seen_order_columns.push(column.clone());
            true
        }
    });
    let order_by: Vec<Value> = order_by
        .into_iter()
        .map(|(column, direction)| {
            let mut entry = serde_json::Map::new();
            entry.insert("ref".to_string(), json!({"type": "id", "id": column}));
            entry.insert("direction".to_string(), json!(direction));
            if !is_tree_request {
                entry.insert("block_id".to_string(), json!(0));
            }
            Value::Object(entry)
        })
        .collect();
    let updates: Vec<Value> = shared
        .get("updates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|update| {
            update
                .pointer("/field/datasetId")
                .and_then(Value::as_str)
                .map(|update_dataset| update_dataset == dataset)
                .unwrap_or(true)
        })
        .collect();
    let page = table_param_u64(params, "_page").unwrap_or(1);
    let mut body = serde_json::Map::new();
    body.insert("fields".to_string(), Value::Array(request_fields));
    body.insert("filters".to_string(), Value::Array(filters));
    body.insert("order_by".to_string(), Value::Array(order_by));
    body.insert("parameter_values".to_string(), Value::Array(parameter_values));
    body.insert("dataset_revision_id".to_string(), revision.clone());
    if !updates.is_empty() {
        body.insert("updates".to_string(), Value::Array(updates));
    }
    body.insert("ignore_nonexistent_filters".to_string(), json!(true));
    if pagination_on {
        if let Some(limit) = limit {
            body.insert("limit".to_string(), json!(limit));
            body.insert(
                "offset".to_string(),
                json!(limit.saturating_mul(page.saturating_sub(1))),
            );
        }
    }
    body.insert("with_totals".to_string(), json!(with_totals));
    body.insert("autofill_legend".to_string(), json!(true));
    Value::Object(body)
}

/// Builds the `data` member (`prepareFlatTable`, `getFooter`, background CSS).
fn table_build_data(
    shared: &Value,
    fields: &[Value],
    mut rows: Vec<Value>,
    columns: &[&Value],
    drill: Option<&TableDrill>,
    params: &Value,
    lang: &str,
) -> Value {
    let mut titles: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut data_types: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for field in fields {
        let Some(id) = field.get("id").and_then(Value::as_str) else {
            continue;
        };
        titles.entry(id.to_string()).or_insert_with(|| {
            field
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        });
        data_types.entry(id.to_string()).or_insert_with(|| {
            field
                .get("data_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        });
    }

    let mut totals_by_id: Option<std::collections::HashMap<String, Value>> = None;
    if let Some(last) = rows.last() {
        let legend = table_legend_ids(last);
        let mut is_totals = false;
        let mut is_placeholder = false;
        for legend_id in &legend {
            if let Some(field) = table_field_by_legend(fields, *legend_id) {
                match table_field_role(field) {
                    "total" => is_totals = true,
                    "template" => is_placeholder = true,
                    _ => {}
                }
            }
        }
        if is_totals || (legend.len() == 1 && is_placeholder) {
            let row = rows.pop().unwrap_or(Value::Null);
            if is_totals {
                let mut totals = std::collections::HashMap::new();
                let data = row
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for (position, legend_id) in legend.iter().enumerate() {
                    if let Some(field) = table_field_by_legend(fields, *legend_id) {
                        if let Some(id) = field.get("id").and_then(Value::as_str) {
                            totals.insert(
                                id.to_string(),
                                data.get(position).cloned().unwrap_or(Value::Null),
                            );
                        }
                    }
                }
                totals_by_id = Some(totals);
            }
        }
    }
    let first_legend = rows.first().map(table_legend_ids).unwrap_or_default();
    let mut index_cache: std::collections::HashMap<String, Option<usize>> =
        std::collections::HashMap::new();
    {
        for column in columns {
            if let Some(guid) = column.get("guid").and_then(Value::as_str) {
                let found = table_index_by_guid(fields, &first_legend, guid);
                index_cache.insert(guid.to_string(), found);
            }
        }
    }
    let index_for = |guid: &str| -> Option<usize> {
        index_cache.get(guid).copied().flatten()
    };
    let data_type_for = |item: &Value| -> String {
        let guid = item.get("guid").and_then(|value| value.as_str()).unwrap_or("");
        data_types
            .get(guid)
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| table_data_type(item).to_string())
    };
    let title_for = |item: &Value| -> String {
        let guid = item.get("guid").and_then(|value| value.as_str()).unwrap_or("");
        let fake = item.get("fakeTitle").and_then(|value| value.as_str()).unwrap_or("");
        if !fake.is_empty() {
            return fake.to_string();
        }
        titles
            .get(guid)
            .cloned()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                item.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
    };
    // Column metadata is invariant for the whole table. Precompute it once; before this
    // `data_type_for` allocated a String (and did a HashMap lookup) per cell.
    let column_meta: Vec<Option<(String, String)>> = columns
        .iter()
        .map(|item| {
            item.get("guid")
                .and_then(Value::as_str)
                .map(|_| (title_for(item), data_type_for(item)))
        })
        .collect();
    // values per bar column (bar scale; the totals value joins when configured)
    let mut column_values: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for item in columns {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        if !item
            .pointer("/barsSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(index) = index_for(guid) else {
            continue;
        };
        let mut values: Vec<Value> = rows
            .iter()
            .filter_map(|row| {
                row.get("data")
                    .and_then(Value::as_array)
                    .and_then(|data| data.get(index))
                    .cloned()
            })
            .collect();
        if item
            .pointer("/barsSettings/showBarsInTotals")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if let Some(totals) = &totals_by_id {
                if let Some(total) = totals.get(guid) {
                    values.push(total.clone());
                }
            }
        }
        column_values.insert(guid.to_string(), values);
    }
    // background colours: continuous gradients and discrete palette-state mappings
    let mut background_colors: std::collections::HashMap<String, (bool, Vec<Option<String>>)> =
        std::collections::HashMap::new();
    for item in columns {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        let Some(background) = item.get("backgroundSettings") else {
            continue;
        };
        if !background
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let continuous = background
            .pointer("/settings/isContinuous")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if continuous {
            // `getBackgroundColorsMapByContinuousColumn`: the gradient is computed from the
            // `colorFieldGuid` column values (which may differ from the styled column).
            let Some(color_field_guid) = background
                .get("colorFieldGuid")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(index) = index_for(color_field_guid)
                .or_else(|| table_index_by_guid(fields, &first_legend, color_field_guid))
            else {
                continue;
            };
            let values: Vec<Option<f64>> = rows
                .iter()
                .map(|row| {
                    row.get("data")
                        .and_then(Value::as_array)
                        .and_then(|data| data.get(index))
                        .and_then(table_parse_number)
                })
                .collect();
            background_colors.insert(
                guid.to_string(),
                (true, table_background_colors(&values, background)),
            );
        } else {
            // discrete `paletteState`: colour from the colour-field value of the row
            let Some(color_field_guid) = background
                .get("colorFieldGuid")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(color_index) = index_for(color_field_guid)
                .or_else(|| table_index_by_guid(fields, &first_legend, color_field_guid))
            else {
                continue;
            };
            let colors: Vec<Option<String>> = rows
                .iter()
                .map(|row| {
                    let value = row
                        .get("data")
                        .and_then(Value::as_array)
                        .and_then(|data| data.get(color_index))
                        .cloned()
                        .unwrap_or(Value::Null);
                    if value.is_null() {
                        None
                    } else {
                        table_discrete_background_color(background, &value)
                    }
                })
                .collect();
            background_colors.insert(guid.to_string(), (false, colors));
        }
    }
    let column_bars: Vec<bool> = columns
        .iter()
        .map(|item| {
            item.pointer("/barsSettings/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect();
    // Borrow the colour vectors instead of cloning them per column: a continuous
    // gradient can have one string per row, and the clone was O(rows × columns).
    let column_background: Vec<Option<&(bool, Vec<Option<String>>)>> = columns
        .iter()
        .map(|item| {
            item.get("guid")
                .and_then(Value::as_str)
                .and_then(|guid| background_colors.get(guid))
        })
        .collect();
    let tree_state = table_tree_state(params);
    // head (`prepareFlatTable`)
    let pinned_columns = shared
        .pointer("/extraSettings/pinnedColumns")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let mut head: Vec<Value> = Vec::new();
    for (index, item) in columns.iter().enumerate() {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        let Some((title, data_type)) = column_meta.get(index).and_then(Option::as_ref) else {
            continue;
        };
        let mut cell = serde_json::Map::new();
        cell.insert("id".to_string(), json!(guid));
        cell.insert("name".to_string(), json!(title));
        cell.insert("type".to_string(), json!("text"));
        if let Some(width) = table_column_width(item) {
            cell.insert("width".to_string(), json!(width));
        }
        if table_is_number_type(data_type) {
            cell.insert("type".to_string(), json!("number"));
            cell.insert("formatter".to_string(), json!({}));
            cell.insert("view".to_string(), json!("number"));
            if column_bars.get(index).copied().unwrap_or(false) {
                let values = column_values.get(guid).cloned().unwrap_or_default();
                for (key, value) in table_bars_view_options(item, &values) {
                    cell.insert(key, value);
                }
            } else {
                let options = table_format_options(item);
                table_apply_formatter(&mut cell, &options, data_type);
            }
        } else if table_is_date_type(data_type) {
            cell.insert("type".to_string(), json!("date"));
            let format = item
                .get("format")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match data_type.as_str() {
                    "genericdatetime" | "datetime" => TABLE_DATETIME_FORMAT.to_string(),
                    "datetimetz" => TABLE_DATETIMETZ_FORMAT.to_string(),
                    _ => TABLE_DATE_FORMAT.to_string(),
                });
            cell.insert("format".to_string(), json!(format));
        }
        // `flat-table/index.ts:106-112`: a hint on the head cell, pinned left columns.
        if item
            .pointer("/hintSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if let Some(hint) = item.pointer("/hintSettings/text") {
                cell.insert("hint".to_string(), hint.clone());
            }
        }
        if index + 1 < columns.len() && (index as i64) < pinned_columns {
            cell.insert("pinned".to_string(), json!(true));
        }
        head.push(Value::Object(cell));
    }
    let drill_index = drill
        .and_then(|drill| table_index_by_guid(fields, &first_legend, drill.active_guid()));
    // rows (`prepareFlatTable`)
    let mut out_rows: Vec<Value> = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let data = row
            .get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut cells: Vec<Value> = Vec::with_capacity(columns.len());
        for (column_index, item) in columns.iter().enumerate() {
            let Some(guid) = item.get("guid").and_then(Value::as_str) else {
                continue;
            };
            let data_type = column_meta
                .get(column_index)
                .and_then(Option::as_ref)
                .map(|(_, data_type)| data_type.as_str())
                .unwrap_or("");
            let raw = index_for(guid)
                .and_then(|index| data.get(index))
                .cloned()
                .unwrap_or(Value::Null);
            let mut cell = serde_json::Map::with_capacity(4);
            cell.insert("fieldId".to_string(), json!(guid));
            if raw.is_null() {
                cell.insert("value".to_string(), raw);
            } else if raw.is_array() {
                cell.insert(
                    "value".to_string(),
                    json!(serde_json::to_string(&raw).unwrap_or_default()),
                );
            } else if table_is_markup_type(data_type) {
                cell.insert("type".to_string(), json!("markup"));
                cell.insert("value".to_string(), raw);
            } else if table_is_number_type(data_type) {
                cell.insert("type".to_string(), json!("number"));
                if column_bars.get(column_index).copied().unwrap_or(false) {
                    let (value, formatted_value, bar_color, _) = table_bar_cell(item, &raw);
                    cell.insert("value".to_string(), value);
                    cell.insert("formattedValue".to_string(), json!(formatted_value));
                    cell.insert("barColor".to_string(), bar_color);
                } else {
                    let value = table_parse_number(&raw)
                        .map(metric_json_number)
                        .unwrap_or_else(|| raw.clone());
                    cell.insert("value".to_string(), value);
                }
            } else if table_is_tree_type(data_type) {
                let text = table_string(&raw);
                cell.insert("value".to_string(), json!(text));
                if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(&text) {
                    if let Some(last) = parsed.last() {
                        // The reference sets `cell.treeNode = String(cell.value)`, i.e. the raw
                        // data-api value text (Python renders it with ", " separators). The UI
                        // then sends exactly that string back in `treeState`, so keeping the raw
                        // text makes the whole round-trip identical to the original stack —
                        // `tree_state_contains_path` still compares parsed JSON, so legacy
                        // compact `treeState` entries keep working too.
                        cell.insert("treeNode".to_string(), json!(text));
                        cell.insert("treeOffset".to_string(), json!(parsed.len()));
                        cell.insert(
                            "treeNodeState".to_string(),
                            json!(if tree_state_contains_path(&tree_state, &parsed) {
                                "open"
                            } else {
                                "closed"
                            }),
                        );
                        cell.insert("value".to_string(), last.clone());
                    }
                }
            } else {
                cell.insert("value".to_string(), raw);
            }
            if let (Some(_), Some(index)) = (drill, drill_index) {
                if !table_is_markup_type(data_type) {
                    let value = data.get(index).cloned().unwrap_or(Value::Null);
                    if value.is_null() {
                        cell.insert("drillDownFilterValue".to_string(), json!("__ISNULL_"));
                    } else if !value.is_object() {
                        cell.insert(
                            "drillDownFilterValue".to_string(),
                            json!(table_string(&value)),
                        );
                    }
                }
            }
            if let Some((continuous, colors)) = column_background
                .get(column_index)
                .and_then(Option::as_ref)
            {
                match colors.get(row_index).and_then(|color| color.as_ref()) {
                    Some(color) => {
                        cell.insert(
                            "css".to_string(),
                            json!({"backgroundColor": color, "color": "#FFF"}),
                        );
                    }
                    // `getContinuousBackgroundColorStyle` falls back to an empty css object;
                    // the discrete variant omits `css` when the value has no mounted colour.
                    None if *continuous => {
                        cell.insert("css".to_string(), json!({}));
                    }
                    None => {}
                }
            }
            cells.push(Value::Object(cell));
        }
        out_rows.push(json!({"cells": cells}));
    }
    // footer (`getFooter`)
    let extra = shared.get("extraSettings").unwrap_or(&Value::Null);
    let pagination_disabled = extra.get("pagination").and_then(Value::as_str) != Some("on");
    let limit = extra.get("limit").and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    });
    let page = table_param_u64(params, "_page").unwrap_or(1);
    let should_disable_paginator = page == 1
        && limit
            .map(|limit| limit > 0 && limit as usize > out_rows.len())
            .unwrap_or(false);
    let footer_enabled = !((pagination_disabled || should_disable_paginator)
        && out_rows.len() == 1);
    let mut footer: Vec<Value> = Vec::new();
    if footer_enabled {
        if let Some(totals) = &totals_by_id {
            let mut cells: Vec<Value> = Vec::new();
            for (column_index, item) in columns.iter().enumerate() {
                let Some(guid) = item.get("guid").and_then(Value::as_str) else {
                    continue;
                };
                let data_type = column_meta
                    .get(column_index)
                    .and_then(Option::as_ref)
                    .map(|(_, data_type)| data_type.as_str())
                    .unwrap_or("");
                let total = totals.get(guid).cloned().unwrap_or(Value::Null);
                let mut value = if total.is_null() || table_string(&total).is_empty() {
                    Value::String(String::new())
                } else if table_is_markup_type(data_type) {
                    total.clone()
                } else if table_is_number_type(data_type) {
                    table_parse_number(&total)
                        .map(metric_json_number)
                        .unwrap_or_else(|| total.clone())
                } else {
                    total.clone()
                };
                let mut cell = serde_json::Map::new();
                if column_index == 0 {
                    if table_string(&value).is_empty() {
                        value = json!(if lang == "ru" { "Итого" } else { "Total" });
                    }
                    cell.insert("value".to_string(), value);
                    cell.insert(
                        "css".to_string(),
                        json!({"background-color": TABLE_TOTALS_BG, "font-weight": 500}),
                    );
                    cell.insert("type".to_string(), json!("text"));
                } else {
                    cell.insert("value".to_string(), value);
                    cell.insert(
                        "css".to_string(),
                        json!({"background-color": TABLE_TOTALS_BG, "font-weight": 500}),
                    );
                }
                if column_bars.get(column_index).copied().unwrap_or(false) {
                    let row_value = json!(table_string(&total));
                    let (bar_value, formatted_value, bar_color, show_bar) =
                        table_bar_cell(item, &row_value);
                    cell.insert("value".to_string(), bar_value);
                    cell.insert("formattedValue".to_string(), json!(formatted_value));
                    cell.insert("barColor".to_string(), bar_color);
                    cell.insert("showBar".to_string(), json!(show_bar));
                }
                cells.push(Value::Object(cell));
            }
            footer.push(json!({"cells": cells}));
        }
    }
    let mut data = serde_json::Map::new();
    data.insert("head".to_string(), Value::Array(head));
    data.insert("rows".to_string(), Value::Array(out_rows));
    if !footer.is_empty() {
        data.insert("footer".to_string(), Value::Array(footer));
    }
    Value::Object(data)
}


// ---------------------------------------------------------------------------
// Typed/streaming table data
// ---------------------------------------------------------------------------
//
// The typed path stores only column descriptors + the borrowed head. Rows and cells are
// serialized lazily from the data-api `Value` body: no `Vec<TypedCell>` tree exists.
// The legacy `Value` builder remains the fallback for totals/footer shapes.

#[derive(Clone)]
struct StreamColumn<'a> {
    guid: &'a str,
    data_type: String,
    index: Option<usize>,
    bars: bool,
    item: &'a Value,
    tree: bool,
    markup: bool,
    background: Option<(bool, Vec<Option<String>>)>,
}

pub struct StreamTableData<'a> {
    head: Vec<Value>,
    rows: &'a [Value],
    columns: Vec<StreamColumn<'a>>,
    drill_index: Option<usize>,
    tree_state: Vec<String>,
    footer: Vec<Value>,
}

impl StreamTableData<'_> {
    fn rows_len(&self) -> usize {
        self.rows.len()
    }
}



fn write_metric_number(out: &mut Vec<u8>, value: f64) {
    if !value.is_finite() {
        out.extend_from_slice(b"null");
    } else if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        if serde_json::to_writer(&mut *out, &(value as i64)).is_err() {
            out.extend_from_slice(b"0");
        }
    } else if serde_json::to_writer(&mut *out, &value).is_err() {
        out.extend_from_slice(b"null");
    }
}

/// Direct `barColor`/`value`/`formattedValue` writer for a bar-enabled measure cell.
/// Avoids building the (Value, String, Value, bool) tuple from `table_bar_cell`.
fn write_bar_fields(out: &mut Vec<u8>, item: &Value, row_value: &Value) {
    let bars = item.get("barsSettings").cloned().unwrap_or_else(|| json!({}));
    let color_settings = bars
        .pointer("/colorSettings/settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let color_type = bars
        .pointer("/colorSettings/colorType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bar_color = match color_type {
        "one-color" => color_settings.get("color").cloned().unwrap_or(Value::Null),
        "two-color" => {
            let positive = table_parse_number(row_value)
                .map(|number| number >= 0.0)
                .unwrap_or(true);
            color_settings
                .get(if positive { "positiveColor" } else { "negativeColor" })
                .cloned()
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    let format_options = table_format_options(item);
    out.extend_from_slice(br#","value":"#);
    match table_parse_number(row_value) {
        Some(number) => write_metric_number(out, number),
        None => out.extend_from_slice(b"0"),
    }
    out.extend_from_slice(br#","formattedValue":"#);
    let formatted = match table_parse_number(row_value) {
        Some(number) => table_format_number(number, &format_options),
        None => table_string(row_value),
    };
    write_json_string(out, &formatted);
    out.extend_from_slice(br#","barColor":"#);
    write_json_value(out, &bar_color);
}

fn write_json_value(out: &mut Vec<u8>, value: &Value) {
    if serde_json::to_writer(&mut *out, value).is_err() {
        out.extend_from_slice(b"null");
    }
}

fn write_json_string(out: &mut Vec<u8>, value: &str) {
    if serde_json::to_writer(&mut *out, value).is_err() {
        out.extend_from_slice(b"\"\"");
    }
}

impl StreamTableData<'_> {
    /// Direct JSON writer: fixed keys go through `extend_from_slice`, values are serialized
    /// straight into the destination buffer. No per-cell `Serialize` map and no Value tree.
    fn into_raw_json(self) -> Option<Box<serde_json::value::RawValue>> {
        let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
        out.extend_from_slice(br#"{"head":"#);
        write_json_value(&mut out, &Value::Array(self.head));
        out.extend_from_slice(br#","rows":["#);
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push(b',');
            }
            out.extend_from_slice(br#"{"cells":["#);
            let data = row
                .get("data")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            for (column_index, column) in self.columns.iter().enumerate() {
                if column_index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(br#"{"fieldId":"#);
                write_json_string(&mut out, column.guid);
                let raw = column
                    .index
                    .and_then(|index| data.get(index))
                    .unwrap_or(&Value::Null);
                if raw.is_null() {
                    out.extend_from_slice(br#","value":null"#);
                } else if raw.is_array() {
                    let text = serde_json::to_string(raw).unwrap_or_default();
                    out.extend_from_slice(br#","value":"#);
                    write_json_string(&mut out, &text);
                } else if column.markup {
                    out.extend_from_slice(br#","type":"markup","value":"#);
                    write_json_value(&mut out, raw);
                } else if column.tree {
                    let text = table_string(raw);
                    out.extend_from_slice(br#","value":"#);
                    write_json_string(&mut out, &text);
                    if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(&text) {
                        if let Some(last) = parsed.last() {
                            out.extend_from_slice(br#","treeNode":"#);
                            write_json_string(&mut out, &text);
                            out.extend_from_slice(br#","treeOffset":"#);
                            write_json_value(&mut out, &json!(parsed.len()));
                            out.extend_from_slice(br#","treeNodeState":"#);
                            write_json_string(
                                &mut out,
                                if tree_state_contains_path(&self.tree_state, &parsed) {
                                    "open"
                                } else {
                                    "closed"
                                },
                            );
                            out.extend_from_slice(br#","value":"#);
                            write_json_value(&mut out, last);
                        }
                    }
                } else if table_is_number_type(&column.data_type) {
                    out.extend_from_slice(br#","type":"number""#);
                    if column.bars {
                        write_bar_fields(&mut out, column.item, raw);
                    } else if let Some(number) = table_parse_number(raw) {
                        out.extend_from_slice(br#","value":"#);
                        write_metric_number(&mut out, number);
                    } else {
                        out.extend_from_slice(br#","value":"#);
                        write_json_value(&mut out, raw);
                    }
                } else {
                    out.extend_from_slice(br#","value":"#);
                    write_json_value(&mut out, raw);
                }
                if let Some(index) = self.drill_index {
                    let value = data.get(index).unwrap_or(&Value::Null);
                    if value.is_null() {
                        out.extend_from_slice(br#","drillDownFilterValue":"__ISNULL_""#);
                    } else if !value.is_object() {
                        out.extend_from_slice(br#","drillDownFilterValue":"#);
                        write_json_string(&mut out, &table_string(value));
                    }
                }
                if let Some((continuous, colors)) = &column.background {
                    match colors.get(row_index).and_then(|color| color.as_ref()) {
                        Some(color) => {
                            out.extend_from_slice(br#","css":"#);
                            write_json_value(
                                &mut out,
                                &json!({"backgroundColor": color, "color": "#FFF"}),
                            );
                        }
                        None if *continuous => {
                            out.extend_from_slice(br#","css":{}"#);
                        }
                        None => {}
                    }
                }
                out.push(b'}');
            }
            out.extend_from_slice(b"]}");
        }
        out.extend_from_slice(b"]");
        if !self.footer.is_empty() {
            out.extend_from_slice(br#","footer":"#);
            write_json_value(&mut out, &Value::Array(self.footer));
        }
        out.push(b'}');
        let text = String::from_utf8(out).ok()?;
        serde_json::value::RawValue::from_string(text).ok()
    }
}

/// Envelope `data` member: the legacy `Value` tree or the streaming typed table.
pub enum TableData<'a> {
    Legacy(Value),
    Typed(StreamTableData<'a>),
    Raw(Box<serde_json::value::RawValue>),
}

impl serde::Serialize for TableData<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            TableData::Legacy(value) => value.serialize(serializer),
            TableData::Typed(value) => value.serialize(serializer),
            TableData::Raw(value) => value.serialize(serializer),
        }
    }
}

impl serde::Serialize for StreamTableData<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("head", &self.head)?;
        map.serialize_entry("rows", &RowsRef { table: self })?;
        map.end()
    }
}

struct RowsRef<'a, 'b> {
    table: &'b StreamTableData<'a>,
}
impl serde::Serialize for RowsRef<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.table.rows.len()))?;
        for (row_index, row) in self.table.rows.iter().enumerate() {
            seq.serialize_element(&RowRef { table: self.table, row, row_index })?;
        }
        seq.end()
    }
}

struct RowRef<'a, 'b> {
    table: &'b StreamTableData<'a>,
    row: &'a Value,
    row_index: usize,
}
impl serde::Serialize for RowRef<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(
            "cells",
            &CellsRef { table: self.table, row: self.row, row_index: self.row_index },
        )?;
        map.end()
    }
}

struct CellsRef<'a, 'b> {
    table: &'b StreamTableData<'a>,
    row: &'a Value,
    row_index: usize,
}
impl serde::Serialize for CellsRef<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.table.columns.len()))?;
        for column in &self.table.columns {
            seq.serialize_element(&CellRef {
                table: self.table,
                row: self.row,
                row_index: self.row_index,
                column,
            })?;
        }
        seq.end()
    }
}

struct CellRef<'a, 'b> {
    table: &'b StreamTableData<'a>,
    row: &'a Value,
    row_index: usize,
    column: &'b StreamColumn<'a>,
}

impl serde::Serialize for CellRef<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let raw = self
            .row
            .get("data")
            .and_then(Value::as_array)
            .and_then(|data| self.column.index.and_then(|index| data.get(index)))
            .unwrap_or(&Value::Null);
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("fieldId", self.column.guid)?;
        if raw.is_null() {
            map.serialize_entry("value", &Value::Null)?;
        } else if raw.is_array() {
            let text = serde_json::to_string(raw).unwrap_or_default();
            map.serialize_entry("value", &text)?;
        } else if self.column.markup {
            map.serialize_entry("type", "markup")?;
            map.serialize_entry("value", raw)?;
        } else if self.column.tree {
            let text = table_string(raw);
            map.serialize_entry("value", &text)?;
            if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(&text) {
                if let Some(last) = parsed.last() {
                    map.serialize_entry("treeNode", &text)?;
                    map.serialize_entry("treeOffset", &parsed.len())?;
                    map.serialize_entry(
                        "treeNodeState",
                        if tree_state_contains_path(&self.table.tree_state, &parsed) {
                            "open"
                        } else {
                            "closed"
                        },
                    )?;
                    map.serialize_entry("value", last)?;
                }
            }
        } else if table_is_number_type(&self.column.data_type) {
            map.serialize_entry("type", "number")?;
            if self.column.bars {
                let (value, formatted_value, bar_color, _) =
                    table_bar_cell(self.column.item, raw);
                map.serialize_entry("value", &value)?;
                map.serialize_entry("formattedValue", &formatted_value)?;
                map.serialize_entry("barColor", &bar_color)?;
            } else if let Some(number) = table_parse_number(raw) {
                map.serialize_entry("value", &metric_json_number(number))?;
            } else {
                map.serialize_entry("value", raw)?;
            }
        } else {
            map.serialize_entry("value", raw)?;
        }
        if let Some(index) = self.table.drill_index {
            let data = self
                .row
                .get("data")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let value = data.get(index).unwrap_or(&Value::Null);
            if value.is_null() {
                map.serialize_entry("drillDownFilterValue", "__ISNULL_")?;
            } else if !value.is_object() {
                map.serialize_entry("drillDownFilterValue", &table_string(value))?;
            }
        }
        if let Some((continuous, colors)) = &self.column.background {
            match colors.get(self.row_index).and_then(|color| color.as_ref()) {
                Some(color) => {
                    map.serialize_entry("css", &json!({"backgroundColor": color, "color": "#FFF"}))?;
                }
                None if *continuous => {
                    map.serialize_entry("css", &json!({}))?;
                }
                None => {}
            }
        }
        map.end()
    }
}

fn table_typed_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        // Direct token writer/footer experiment: contract-complete but the quiet-host A/B
        // was neutral (v202 vs v169 within noise). Keep it available with TABLE_TYPED=1;
        // the legacy Value builder is the safe default.
        std::env::var("TABLE_TYPED")
            .map(|value| value == "1" || value == "true")
            .unwrap_or(false)
    })
}

/// Build the shared `head` member. Used by both the legacy and streaming builders so the
/// head contract cannot drift.
fn table_build_head(
    columns: &[&Value],
    column_meta: &[Option<(String, String)>],
    column_values: &std::collections::HashMap<String, Vec<Value>>,
    column_bars: &[bool],
    pinned_columns: i64,
) -> Vec<Value> {
    let mut head: Vec<Value> = Vec::with_capacity(columns.len());
    for (index, item) in columns.iter().enumerate() {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        let Some((title, data_type)) = column_meta.get(index).and_then(Option::as_ref) else {
            continue;
        };
        let mut cell = serde_json::Map::new();
        cell.insert("id".to_string(), json!(guid));
        cell.insert("name".to_string(), json!(title));
        cell.insert("type".to_string(), json!("text"));
        if let Some(width) = table_column_width(item) {
            cell.insert("width".to_string(), json!(width));
        }
        if table_is_number_type(data_type) {
            cell.insert("type".to_string(), json!("number"));
            cell.insert("formatter".to_string(), json!({}));
            cell.insert("view".to_string(), json!("number"));
            if column_bars.get(index).copied().unwrap_or(false) {
                let values = column_values.get(guid).cloned().unwrap_or_default();
                for (key, value) in table_bars_view_options(item, &values) {
                    cell.insert(key, value);
                }
            } else {
                let options = table_format_options(item);
                table_apply_formatter(&mut cell, &options, data_type);
            }
        } else if table_is_date_type(data_type) {
            cell.insert("type".to_string(), json!("date"));
            let format = item
                .get("format")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| match data_type.as_str() {
                    "genericdatetime" | "datetime" => TABLE_DATETIME_FORMAT.to_string(),
                    "datetimetz" => TABLE_DATETIMETZ_FORMAT.to_string(),
                    _ => TABLE_DATE_FORMAT.to_string(),
                });
            cell.insert("format".to_string(), json!(format));
        }
        if item
            .pointer("/hintSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if let Some(hint) = item.pointer("/hintSettings/text") {
                cell.insert("hint".to_string(), hint.clone());
            }
        }
        if index + 1 < columns.len() && (index as i64) < pinned_columns {
            cell.insert("pinned".to_string(), json!(true));
        }
        head.push(Value::Object(cell));
    }
    head
}

fn table_build_typed_footer(
    shared: &Value,
    params: &Value,
    columns: &[&Value],
    column_meta: &[Option<(String, String)>],
    column_bars: &[bool],
    totals_by_id: Option<&std::collections::HashMap<String, Value>>,
    rows_len: usize,
    lang: &str,
) -> Vec<Value> {
    let extra = shared.get("extraSettings").unwrap_or(&Value::Null);
    let pagination_disabled = extra.get("pagination").and_then(Value::as_str) != Some("on");
    let limit = extra.get("limit").and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    });
    let page = table_param_u64(params, "_page").unwrap_or(1);
    let should_disable_paginator = page == 1
        && limit
            .map(|limit| limit > 0 && limit as usize > rows_len)
            .unwrap_or(false);
    let footer_enabled = !((pagination_disabled || should_disable_paginator) && rows_len == 1);
    let mut footer: Vec<Value> = Vec::new();
    if footer_enabled {
        if let Some(totals) = totals_by_id {
            let mut cells: Vec<Value> = Vec::new();
            for (column_index, item) in columns.iter().enumerate() {
                let Some(guid) = item.get("guid").and_then(Value::as_str) else {
                    continue;
                };
                let data_type = column_meta
                    .get(column_index)
                    .and_then(Option::as_ref)
                    .map(|(_, data_type)| data_type.as_str())
                    .unwrap_or("");
                let total = totals.get(guid).cloned().unwrap_or(Value::Null);
                let mut value = if total.is_null() || table_string(&total).is_empty() {
                    Value::String(String::new())
                } else if table_is_markup_type(data_type) {
                    total.clone()
                } else if table_is_number_type(data_type) {
                    table_parse_number(&total)
                        .map(metric_json_number)
                        .unwrap_or_else(|| total.clone())
                } else {
                    total.clone()
                };
                let mut cell = serde_json::Map::new();
                if column_index == 0 {
                    if table_string(&value).is_empty() {
                        value = json!(if lang == "ru" { "Итого" } else { "Total" });
                    }
                    cell.insert("value".to_string(), value);
                    cell.insert(
                        "css".to_string(),
                        json!({"background-color": TABLE_TOTALS_BG, "font-weight": 500}),
                    );
                    cell.insert("type".to_string(), json!("text"));
                } else {
                    cell.insert("value".to_string(), value);
                    cell.insert(
                        "css".to_string(),
                        json!({"background-color": TABLE_TOTALS_BG, "font-weight": 500}),
                    );
                }
                if column_bars.get(column_index).copied().unwrap_or(false) {
                    let row_value = json!(table_string(&total));
                    let (bar_value, formatted_value, bar_color, show_bar) =
                        table_bar_cell(item, &row_value);
                    cell.insert("value".to_string(), bar_value);
                    cell.insert("formattedValue".to_string(), json!(formatted_value));
                    cell.insert("barColor".to_string(), bar_color);
                    cell.insert("showBar".to_string(), json!(show_bar));
                }
                cells.push(Value::Object(cell));
            }
            footer.push(json!({"cells": cells}));
        }
    }
    footer
}

fn table_build_typed<'a>(
    shared: &Value,
    fields: &[Value],
    rows: &'a [Value],
    columns: &'a [&'a Value],
    params: &Value,
    lang: &str,
    drill: Option<&TableDrill>,
) -> Option<StreamTableData<'a>> {
    let mut column_meta: Vec<Option<(String, String)>> = Vec::with_capacity(columns.len());
    let mut column_bars: Vec<bool> = Vec::with_capacity(columns.len());
    let mut stream_columns: Vec<StreamColumn<'a>> = Vec::with_capacity(columns.len());
    for item in columns {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            column_meta.push(None);
            column_bars.push(false);
            continue;
        };
        let field = fields
            .iter()
            .find(|field| field.get("id").and_then(Value::as_str) == Some(guid));
        let data_type = field
            .and_then(|field| field.get("data_type").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| table_data_type(item))
            .to_string();
        let tree = table_is_tree_type(&data_type);
        let markup = table_is_markup_type(&data_type);
        let fake = item
            .get("fakeTitle")
            .and_then(Value::as_str)
            .unwrap_or("");
        let title = if !fake.is_empty() {
            fake.to_string()
        } else {
            field
                .and_then(|field| field.get("title").and_then(Value::as_str))
                .filter(|value| !value.is_empty())
                .or_else(|| item.get("title").and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        let bars = item
            .pointer("/barsSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        column_meta.push(Some((title, data_type.clone())));
        column_bars.push(bars);
        stream_columns.push(StreamColumn {
            guid,
            data_type,
            index: None,
            bars,
            item,
            tree,
            markup,
            background: None,
        });
    }
    // Totals/placeholder rows are not streamed as normal data. The footer is built
    // exactly like the legacy builder, but only once.
    let mut totals_by_id: Option<std::collections::HashMap<String, Value>> = None;
    let mut data_rows: &'a [Value] = rows;
    if let Some(last) = rows.last() {
        let legend = table_legend_ids(last);
        let mut is_totals = false;
        let mut placeholder = false;
        for legend_id in &legend {
            if let Some(field) = table_field_by_legend(fields, *legend_id) {
                match table_field_role(field) {
                    "total" => is_totals = true,
                    "template" => placeholder = true,
                    _ => {}
                }
            }
        }
        if is_totals || (legend.len() == 1 && placeholder) {
            data_rows = &rows[..rows.len() - 1];
        }
        if is_totals {
            let row = last;
            let mut totals = std::collections::HashMap::new();
            let data = row.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            for (position, legend_id) in legend.iter().enumerate() {
                if let Some(field) = table_field_by_legend(fields, *legend_id) {
                    if let Some(id) = field.get("id").and_then(Value::as_str) {
                        totals.insert(id.to_string(), data.get(position).cloned().unwrap_or(Value::Null));
                    }
                }
            }
            totals_by_id = Some(totals);
        }
    }
    let first_legend = data_rows.first().map(table_legend_ids).unwrap_or_default();
    let mut index_cache: std::collections::HashMap<String, Option<usize>> =
        std::collections::HashMap::new();
    for column in columns {
        if let Some(guid) = column.get("guid").and_then(Value::as_str) {
            index_cache.insert(guid.to_string(), table_index_by_guid(fields, &first_legend, guid));
        }
    }
    let index_for = |guid: &str| -> Option<usize> { index_cache.get(guid).copied().flatten() };
    for column in &mut stream_columns {
        column.index = index_for(column.guid);
    }
    let mut background_colors: std::collections::HashMap<String, (bool, Vec<Option<String>>)> =
        std::collections::HashMap::new();
    for item in columns {
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        let Some(background) = item.get("backgroundSettings") else {
            continue;
        };
        if !background
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let continuous = background
            .pointer("/settings/isContinuous")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if continuous {
            let Some(color_field_guid) = background.get("colorFieldGuid").and_then(Value::as_str) else {
                continue;
            };
            let Some(index) = index_for(color_field_guid)
                .or_else(|| table_index_by_guid(fields, &first_legend, color_field_guid))
            else {
                continue;
            };
            let values: Vec<Option<f64>> = data_rows
                .iter()
                .map(|row| {
                    row.get("data")
                        .and_then(Value::as_array)
                        .and_then(|data| data.get(index))
                        .and_then(table_parse_number)
                })
                .collect();
            background_colors.insert(
                guid.to_string(),
                (true, table_background_colors(&values, background)),
            );
        } else {
            let Some(color_field_guid) = background.get("colorFieldGuid").and_then(Value::as_str) else {
                continue;
            };
            let Some(color_index) = index_for(color_field_guid)
                .or_else(|| table_index_by_guid(fields, &first_legend, color_field_guid))
            else {
                continue;
            };
            let colors: Vec<Option<String>> = data_rows
                .iter()
                .map(|row| {
                    let value = row
                        .get("data")
                        .and_then(Value::as_array)
                        .and_then(|data| data.get(color_index))
                        .cloned()
                        .unwrap_or(Value::Null);
                    if value.is_null() {
                        None
                    } else {
                        table_discrete_background_color(background, &value)
                    }
                })
                .collect();
            background_colors.insert(guid.to_string(), (false, colors));
        }
    }
    for column in &mut stream_columns {
        column.background = background_colors.get(column.guid).cloned();
    }
    let mut column_values: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for (index, item) in columns.iter().enumerate() {
        if !column_bars.get(index).copied().unwrap_or(false) {
            continue;
        }
        let Some(guid) = item.get("guid").and_then(Value::as_str) else {
            continue;
        };
        let Some(index) = index_for(guid) else {
            continue;
        };
        let values: Vec<Value> = data_rows
            .iter()
            .filter_map(|row| {
                row.get("data")
                    .and_then(Value::as_array)
                    .and_then(|data| data.get(index))
                    .cloned()
            })
            .collect();
        column_values.insert(guid.to_string(), values);
    }
    let pinned_columns = shared
        .pointer("/extraSettings/pinnedColumns")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let head = table_build_head(columns, &column_meta, &column_values, &column_bars, pinned_columns);
    let drill_index = drill
        .and_then(|drill| table_index_by_guid(fields, &first_legend, drill.active_guid()));
    let tree_state = table_tree_state(params);
    let footer = table_build_typed_footer(
        shared,
        params,
        columns,
        &column_meta,
        &column_bars,
        totals_by_id.as_ref(),
        data_rows.len(),
        lang,
    );
    Some(StreamTableData {
        head,
        rows: data_rows,
        columns: stream_columns,
        drill_index,
        tree_state,
        footer,
    })
}


/// `buildChartsConfigPrivate` for a flat table plus the prepare-stage overrides.
fn table_config(
    shared: &Value,
    params: &Value,
    drill: Option<&TableDrill>,
    rows_length: usize,
) -> String {
    let extra = shared.get("extraSettings").unwrap_or(&Value::Null);
    let items = table_placeholder_items(shared);
    let has_dimensions = items
        .iter()
        .any(|item| table_is_dimension(item) || table_data_type(item) == "hierarchy");
    let pagination_on =
        has_dimensions && extra.get("pagination").and_then(Value::as_str) == Some("on");
    let limit = extra.get("limit").and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    });
    let page = table_param_u64(params, "_page").unwrap_or(1);
    let should_disable_paginator = page == 1
        && limit
            .map(|limit| limit > 0 && limit as usize > rows_length)
            .unwrap_or(false);
    let paginator_enabled = pagination_on && !should_disable_paginator;
    let matched_params = table_matched_params(shared, params);
    let mut config = String::from("{");
    let mut first = true;
    let mut put = |config: &mut String, key: &str, value: Value| {
        if !first {
            config.push(',');
        }
        first = false;
        config.push('"');
        config.push_str(key);
        config.push_str("\":");
        config.push_str(&serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()));
    };
    if extra.get("titleMode").and_then(Value::as_str) == Some("show") {
        if let Some(title) = extra.get("title").and_then(Value::as_str) {
            put(&mut config, "title", json!(title));
        }
    }
    put(&mut config, "hideHolidaysBands", json!(true));
    put(&mut config, "linesLimit", json!(100));
    put(
        &mut config,
        "tooltip",
        json!({"pin": {"altKey": true}, "sort": {"enabled": true}}),
    );
    put(&mut config, "preventDefaultForPointClick", json!(false));
    let mut paginator = serde_json::Map::new();
    paginator.insert("enabled".to_string(), json!(paginator_enabled));
    if let Some(limit) = limit {
        paginator.insert("limit".to_string(), json!(limit));
    }
    put(&mut config, "paginator", Value::Object(paginator));
    put(
        &mut config,
        "comments",
        json!({"matchType": "intersection", "matchedParams": matched_params}),
    );
    let mut settings = serde_json::Map::new();
    settings.insert("width".to_string(), json!("max-content"));
    put(&mut config, "settings", Value::Object(settings));
    if let Some(drill) = drill {
        put(
            &mut config,
            "drillDown",
            json!({"breadcrumbs": drill.breadcrumbs, "dateFormat": drill.date_format()}),
        );
    }
    config.push('}');
    config
}

/// Flat-table `/api/run` answer, serialized in one pass from borrowed values (same
/// envelope layout as the metric path).
#[derive(serde::Serialize)]
struct TableEnvelope<'a> {
    sources: std::collections::BTreeMap<&'a str, MetricSource<'a>>,
    #[serde(rename = "uiScheme")]
    ui_scheme: Option<()>,
    params: &'a Value,
    #[serde(rename = "usedParams")]
    used_params: &'a Value,
    #[serde(rename = "actionParams")]
    action_params: EmptyObject,
    #[serde(rename = "widgetConfig")]
    widget_config: EmptyObject,
    #[serde(rename = "defaultParams")]
    default_params: EmptyObject,
    extra: std::sync::Arc<Value>,
    timings: MetricTimings,
    data: TableData<'a>,
    config: &'a str,
    #[serde(rename = "highchartsConfig")]
    highcharts_config: &'static str,
    #[serde(rename = "dataExport")]
    data_export: std::collections::BTreeMap<&'a str, &'a Value>,
    #[serde(rename = "_confStorageConfig")]
    conf_storage_config: &'a Value,
    key: &'a Value,
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "revId")]
    rev_id: &'a Value,
}

/// Graph `/api/run` answer; `sources` is the finished graph builder's object,
/// `config`/`highchartsConfig` are the two JSON strings the contract requires.
#[derive(serde::Serialize)]
struct GraphEnvelope<'a> {
    sources: &'a Value,
    #[serde(rename = "uiScheme")]
    ui_scheme: Option<()>,
    params: &'a Value,
    #[serde(rename = "usedParams")]
    used_params: &'a Value,
    #[serde(rename = "actionParams")]
    action_params: EmptyObject,
    #[serde(rename = "widgetConfig")]
    widget_config: EmptyObject,
    #[serde(rename = "defaultParams")]
    default_params: EmptyObject,
    extra: Value,
    timings: MetricTimings,
    data: &'a crate::graph_engine::GraphData,
    config: &'a str,
    #[serde(rename = "highchartsConfig")]
    highcharts_config: &'a str,
    #[serde(rename = "dataExport")]
    data_export: &'a Value,
    #[serde(rename = "_confStorageConfig")]
    conf_storage_config: &'a Value,
    key: &'a Value,
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(rename = "revId")]
    rev_id: &'a Value,
}

/// Move the flat-table `Value` body apart (fields / first-chunk rows / data_export) instead
/// of deep-cloning it; the rest of the parsed body is dropped.
fn take_table_value_parts(mut body: Value) -> (Vec<Value>, Vec<Value>, Value) {
    let fields = match body.get_mut("fields").map(std::mem::take) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let rows = match body.pointer_mut("/result_data/0/rows").map(std::mem::take) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let data_export = match body.get_mut("data_export").map(std::mem::take) {
        Some(value) => value,
        None => json!({}),
    };
    (fields, rows, data_export)
}

/// Flat-table (`flatTable`) Rust path: `shared` must carry a `flatTable` visualization.
async fn run_rust_table(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    lang: &str,
    _entry_id: &str,
    meta_entry_id: Value,
    meta_key: Value,
    meta_rev: Value,
    response_id: &str,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let visualization_id = shared
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if visualization_id != "flatTable" {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            format!("table: visualization {visualization_id} is not ported"),
        ));
    }
    run_table_shared(
        state,
        shared,
        params,
        lang,
        meta_entry_id,
        meta_key,
        meta_rev,
        response_id,
        started,
        scope,
    )
    .await
}

/// The table body/data/envelope builder, shared by the flat-table path and the pivot-table
/// fallback (a pivot shape that is not ported is rendered as a flat table so the widget still
/// shows its data instead of a 501).
#[allow(clippy::too_many_arguments)]
async fn run_table_shared(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    lang: &str,
    meta_entry_id: Value,
    meta_key: Value,
    meta_rev: Value,
    response_id: &str,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let dataset = shared_dataset_ids(shared)
        .into_iter()
        .next()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "table: dataset is missing".to_string()))?;
    let dataset = dataset.as_str();
    // `__relative_*` values must be resolved the same way the JS `resolveParams` step does;
    // the shared helper lives in `pivot_engine` (relative-date resolver).
    let resolved_params = crate::pivot_engine::resolve_params(params)
        .map_err(|message| (StatusCode::BAD_REQUEST, format!("table params: {message}")))?;
    let params = &resolved_params;
    let (expanded, drill) = table_expand_hierarchies(shared, params);
    let columns = table_columns(&expanded);
    if columns.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "table: flat-table-columns is empty".to_string(),
        ));
    }
    // `/fields` pins the dataset revision (the same one V8 puts in the request)
    // and supplies the schema used to classify parameter filters.
    let (dataset_fields, revision) = match dataset_fields(state, scope, dataset).await {
        Ok((fields, revision)) => (fields, revision),
        Err(_) => (Arc::new(json!([])), Arc::new(Value::Null)),
    };
    let request_body = table_request_cached(
        &expanded,
        params,
        dataset,
        &revision,
        &dataset_fields,
        drill.as_ref(),
        &scope.branch,
        &meta_entry_id,
        &meta_rev,
        lang,
    );
    let fetch_started = std::time::Instant::now();
    // v8.2: typed in-process data-api (embedded track) → table fields/rows are moved out of
    // the typed `ResultResponse`; the HTTP fallback moves them out of the parsed `Value`
    // instead of deep-cloning the row batch (the old intro cloned fields and all rows).
    let (status, fields, rows, body_json_len, data_export_value, query, forbidden) =
        if crate::data_direct::installed() {
            match crate::data_direct::result_typed(
                &state.http,
                &state.data_api,
                &scope.headers,
                dataset,
                &request_body,
            )
            .await
            .map_err(|(status, message)| (status, format!("table data-api: {message}")))?
            {
                crate::data_direct::TypedResult::Ok(outcome) => {
                    let status = outcome.status;
                    let size = outcome.json_len as usize;
                    let query = typed_blocks_query(&outcome.body);
                    let forbidden = outcome
                        .body
                        .as_result()
                        .map(|result| result.data_export_forbidden())
                        .unwrap_or(false);
                    let data_export = outcome.data_export;
                    let Some(parts) = outcome.body.into_table_parts(data_export) else {
                        return Err((
                            StatusCode::BAD_GATEWAY,
                            "table data-api: unexpected pivot body".to_string(),
                        ));
                    };
                    (
                        status,
                        parts.fields,
                        parts.rows,
                        size,
                        parts.data_export,
                        query,
                        forbidden,
                    )
                }
                crate::data_direct::TypedResult::Failed { status, body } => {
                    tracing::warn!(%status, body = %body, "table data-api error");
                    return Err(data_fetch_error(&source_key_for(dataset), status.as_u16(), &request_body, &body));
                }
            }
        } else {
            let outcome = crate::data_direct::result(
                &state.http,
                &state.data_api,
                &scope.headers,
                dataset,
                &request_body,
            )
            .await
            .map_err(|(status, message)| (status, format!("table data-api: {message}")))?;
            let status = outcome.status;
            let len = outcome.json_len as usize;
            let body = outcome.body;
            if !status.is_success() {
                tracing::warn!(%status, body = %body, "table data-api error");
                return Err(data_fetch_error(&source_key_for(dataset), status.as_u16(), &request_body, &body));
            }
            let query = blocks_query(&body);
            let forbidden = body
                .get("data_export_forbidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (fields, rows, data_export) = take_table_value_parts(body);
            (status, fields, rows, len, data_export, query, forbidden)
        };
    let fetch_ended = std::time::Instant::now();
    if rows.len() > 100_000 {
        return Err(rows_oversize(100_000, rows.len()));
    }
    let build_started = std::time::Instant::now();
    // Typed/borrowed table fast path. The legacy builder remains the fallback for
    // drill/background/tree/totals shapes and for TABLE_TYPED=0.
    let typed = if table_typed_enabled() {
        table_build_typed(&expanded, &fields, &rows, &columns, params, lang, drill.as_ref())
    } else {
        None
    };
    let config_started = std::time::Instant::now();
    let (data, rows_length) = match typed {
        Some(data) => {
            let rows_length = data.rows_len();
            match data.into_raw_json() {
                Some(raw) => (TableData::Raw(raw), rows_length),
                None => {
                    // Should not happen for a valid table; report a minimal valid data
                    // object rather than breaking the envelope.
                    (TableData::Raw(serde_json::value::RawValue::from_string("{\"head\":[],\"rows\":[]}".to_string()).map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "typed table: raw json".to_string()))?), rows_length)
                }
            }
        }
        None => {
            let data = table_build_data(
                &expanded,
                &fields,
                rows,
                &columns,
                drill.as_ref(),
                params,
                lang,
            );
            let rows_length = data
                .get("rows")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            (TableData::Legacy(data), rows_length)
        }
    };
    let config = table_config(&expanded, params, drill.as_ref(), rows_length);
    record_stage(
        "table.build",
        config_started.elapsed().as_secs_f64() * 1000.0,
    );
    let latency = fetch_ended.duration_since(fetch_started).as_secs_f64() * 1000.0;
    let source_id = format!("{}_result", dataset);
    let source = MetricSource {
        source_id: &source_id,
        source_type: "bi_datasets",
        status: status.as_u16(),
        latency,
        size: body_json_len,
        dataset_id: dataset,
        data: &request_body,
        info: (!query.is_empty()).then(|| json!({"query": query})),
    };
    let (params, used_params) = chart_params(shared, params, &[fields.as_slice()]);
    let extra_key = format!(
        "{}|{}|{}|{}|{}|{}|{}",
        scope.branch,
        meta_entry_id,
        meta_rev,
        dataset,
        revision,
        forbidden,
        state.default_color_palette,
    );
    let extra = chart_extra_cached(
        state,
        shared,
        &json!([{"id": dataset, "fields": fields}]),
        forbidden,
        Some(&extra_key),
    );
    let completed = std::time::Instant::now();
    let timings = MetricTimings::from_native_stages(started, fetch_started, fetch_ended, completed);
    let _elapsed = completed.duration_since(started).as_secs_f64() * 1000.0;
    let conf = json!({
        "type": "table_wizard_node",
        "meta": {"stype": "table_wizard_node"},
        "entryId": meta_entry_id,
        "key": meta_key,
        "revId": meta_rev,
    });
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(source_id.as_str(), source);
    let mut data_export = std::collections::BTreeMap::new();
    data_export.insert(source_id.as_str(), &data_export_value);
    let envelope = TableEnvelope {
        sources,
        ui_scheme: None,
        params: &params,
        used_params: &used_params,
        action_params: EmptyObject {},
        widget_config: EmptyObject {},
        default_params: EmptyObject {},
        extra,
        timings,
        data,
        config: &config,
        highcharts_config: "{}",
        data_export,
        conf_storage_config: &conf,
        key: &meta_key,
        id: response_id,
        kind: "table_wizard_node",
        rev_id: &meta_rev,
    };
    let mut buffer = BytesMut::with_capacity(16 * 1024);
    {
        let mut writer = (&mut buffer).writer();
        serde_json::to_writer(&mut writer, &envelope)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    record_stage("table.reply", build_started.elapsed().as_secs_f64() * 1000.0);
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(response_body(buffer))
        .expect("table response"))
}

/// Pivot table (`pivotTable`) Rust path: the data-api request, the
/// `head`/`rows`/`footer` data and the config/envelope are built in
/// `crate::pivot_engine`. A pivot shape that is not ported (sub-totals, multiple measures,
/// `Measure Names`, ...) falls back to the flat-table builder over the same selection, so the
/// widget shows its data instead of a 501.
#[allow(clippy::too_many_arguments)]
async fn run_rust_pivot(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    lang: &str,
    _entry_id: &str,
    meta_entry_id: Value,
    meta_key: Value,
    meta_rev: Value,
    response_id: &str,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    if let Err(message) = crate::pivot_engine::supports(shared) {
        // A pivot shape the Rust engine does not build (multiple measures, `Measure Names`
        // pseudo dimensions, sub-totals, ...) must not answer `501 chart type ... is not
        // supported`: the dashboard widget would go empty. Render the very same selection
        // through the flat-table builder instead (one column per field, one row per query
        // row) and say so in the log.
        std::hint::cold_path();
        tracing::warn!(%message, "pivot: unsupported shape; flat-table fallback");
        let flat = crate::pivot_engine::flat_table_shared(shared);
        return run_table_shared(
            state,
            &flat,
            params,
            lang,
            meta_entry_id,
            meta_key,
            meta_rev,
            response_id,
            started,
            scope,
        )
        .await;
    }
    let dataset = shared_dataset_ids(shared)
        .into_iter()
        .next()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "pivot: dataset is missing".to_string()))?;
    let dataset = dataset.as_str();
    let (dataset_fields, revision) = dataset_fields(state, scope, dataset).await?;
    let params = crate::pivot_engine::resolve_params(params)
        .map_err(|message| (StatusCode::BAD_REQUEST, format!("pivot params: {message}")))?;
    let request_body =
        crate::pivot_engine::build_request(shared, &params, dataset, &revision, &dataset_fields)
            .map_err(|message| (StatusCode::BAD_REQUEST, format!("pivot request: {message}")))?;
    let fetch_started = std::time::Instant::now();
    let outcome = crate::data_direct::pivot(
        &state.http,
        &state.data_api,
        &scope.headers,
        dataset,
        &request_body,
    )
    .await
    .map_err(|(status, message)| (status, format!("pivot data-api: {message}")))?;
    let status = outcome.status;
    let size = outcome.json_len;
    let body = outcome.body;
    if !status.is_success() {
        return Err(data_fetch_error(
            &source_key_for(dataset),
            status.as_u16(),
            &request_body,
            &body,
        ));
    }
    // `js.ts:433-446,653-668`: backend pivot limits are cells and columns, checked on
    // the raw `pivot_data` before the table is built.
    if let Some(pivot_data) = body.get("pivot_data") {
        let rows = pivot_data
            .get("rows")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let values = pivot_data
            .pointer("/rows/0/values")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let cells = rows * values;
        if cells > 100_000 {
            return Err(table_oversize(true, 100_000, cells));
        }
        let columns = pivot_data
            .get("columns")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if columns > 800 {
            return Err(table_oversize(false, 800, columns));
        }
    }
    let fetch_ended = std::time::Instant::now();
    let build_started = std::time::Instant::now();
    let data = crate::pivot_engine::build_data(shared, &params, &body, lang)
        .map_err(|message| (StatusCode::BAD_REQUEST, format!("pivot data: {message}")))?;
    let rows_length = data
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| rows.len())
        .unwrap_or(0);
    let config = crate::pivot_engine::build_config(shared, &params, rows_length)
        .map_err(|message| (StatusCode::BAD_REQUEST, format!("pivot config: {message}")))?;
    let mut extra = crate::pivot_engine::build_extra(
        shared,
        &json!([{"id": dataset, "fields": body.get("fields").cloned().unwrap_or_else(|| json!([]))}]),
    );
    if let Some(scheme) = palette_scheme(state) {
        if let Some(map) = extra.as_object_mut() {
            map.insert("colors".to_string(), scheme);
        }
    }
    let latency = fetch_ended.duration_since(fetch_started).as_millis() as u64;
    let completed = std::time::Instant::now();
    let elapsed = completed.duration_since(started).as_secs_f64() * 1000.0;
    let config_resolving = fetch_started.duration_since(started).as_secs_f64() * 1000.0;
    let js_execution = (elapsed
        - config_resolving
        - latency as f64)
        .max(0.0);
    let envelope = crate::pivot_engine::build_envelope(crate::pivot_engine::EnvelopeInput {
        params: &params,
        dataset,
        request_body: &request_body,
        response: &body,
        data,
        config,
        extra,
        status: status.as_u16(),
        latency_ms: latency,
        js_execution_ms: js_execution,
        size,
        entry_id: meta_entry_id,
        key: meta_key,
        rev_id: meta_rev,
        response_id,
        elapsed_ms: elapsed,
    })
    .map_err(|message| (StatusCode::BAD_REQUEST, format!("pivot envelope: {message}")))?;
    let mut buffer = BytesMut::with_capacity(16 * 1024);
    {
        let mut writer = (&mut buffer).writer();
        serde_json::to_writer(&mut writer, &envelope)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    }
    record_stage("pivot.build", build_started.elapsed().as_secs_f64() * 1000.0);
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(response_body(buffer))
        .expect("pivot response"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GraphRenderer { Chartkit, Gravity, Unsupported }

fn graph_renderer(shared: &Value) -> GraphRenderer {
    let id = shared.pointer("/visualization/id").and_then(|value| value.as_str()).unwrap_or("");
    match id {
        "funnel" | "pie" | "donut" | "treemap" | "bar" | "bar100p" | "scatter" => GraphRenderer::Gravity,
        "line" | "area" | "area100p" | "column" | "column100p" | "combined-chart" => GraphRenderer::Chartkit,
        _ => GraphRenderer::Unsupported,
    }
}

/// Classic Highcharts chart type a gravity visualization can be converted to when the
/// dedicated gravity builder cannot represent the config. `graph_engine` already builds a
/// full classic `highchartsConfig` for these chartkit types, so the response stays a real
/// non-empty Highcharts envelope instead of a 501.
fn classic_gravity_id(visualization_id: &str) -> Option<&'static str> {
    match visualization_id {
        "bar" => Some("column"),
        "bar100p" => Some("column100p"),
        "scatter" => Some("line"),
        "donut" | "pie" => Some("pie"),
        "treemap" | "funnel" => Some("column"),
        _ => None,
    }
}

fn classic_gravity_shared(shared: &Value, visualization_id: &str, classic_id: &str) -> Value {
    let mut shared = shared.clone();
    if let Some(visualization) = shared.get_mut("visualization") {
        visualization["id"] = json!(classic_id);
        // the classic builder has no gravity-only settings; keep the placeholders/fields
        visualization["layers"] = json!([]);
    }
    let _ = visualization_id;
    shared
}

/// Valid, non-empty Highcharts config for a gravity chart. The gravity client renders from
/// `data`/`config`, but the envelope contract still requires a non-empty `highchartsConfig`,
/// so a classic config is derived from the fetched rows (chart type per visualization).
fn gravity_highcharts_library(visualization_id: &str, response: &Value) -> Value {
    // Borrow the response arrays: the old code deep-cloned `fields`, all rows and the legend
    // only to iterate them, then cloned the selected cells into the generated config.
    let no_fields: &[Value] = &[];
    let no_rows: &[Value] = &[];
    let fields: &[Value] = response
        .get("fields")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_fields);
    let rows: &[Value] = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_rows);
    let no_legend: &[Value] = &[];
    let legend: &[Value] = rows
        .first()
        .and_then(|row| row.get("legend"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_legend);
    let index_by_legend: std::collections::HashMap<i64, usize> = legend
        .iter()
        .enumerate()
        .filter_map(|(index, id)| id.as_i64().map(|id| (id, index)))
        .collect();
    let mut measure_index = None;
    let mut category_index = None;
    let mut second_index = None;
    for field in fields {
        let Some(legend_id) = field.get("legend_item_id").and_then(Value::as_i64) else {
            continue;
        };
        let Some(index) = index_by_legend.get(&legend_id).copied() else {
            continue;
        };
        let data_type = field.get("data_type").and_then(Value::as_str).unwrap_or("");
        if matches!(data_type, "integer" | "uinteger" | "float") {
            if measure_index.is_none() {
                measure_index = Some(index);
            }
        } else if category_index.is_none() {
            category_index = Some(index);
        }
        if second_index.is_none() && Some(index) != category_index && Some(index) != measure_index {
            second_index = Some(index);
        }
    }
    let category_index = category_index.or(measure_index);
    let mut categories: Vec<Value> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    let mut x_values: Vec<Value> = Vec::new();
    let mut y_values: Vec<Value> = Vec::new();
    for row in rows {
        let Some(data) = row.get("data").and_then(Value::as_array) else {
            continue;
        };
        let value = measure_index
            .and_then(|index| data.get(index))
            .cloned()
            .unwrap_or(Value::Null);
        let category = category_index
            .and_then(|index| data.get(index))
            .map(|value| match value {
                Value::String(text) => json!(text),
                other => json!(other.to_string()),
            })
            .unwrap_or_else(|| json!(""));
        categories.push(category);
        values.push(value);
        let x = category_index
            .and_then(|index| data.get(index))
            .map(|value| match value {
                Value::Number(number) => Value::Number(number.clone()),
                other => json!(other.to_string()),
            })
            .unwrap_or(Value::Null);
        let y = measure_index
            .and_then(|index| data.get(index))
            .cloned()
            .unwrap_or(Value::Null);
        x_values.push(x);
        y_values.push(y);
    }
    let chart_type = match visualization_id {
        "bar" | "bar100p" => "bar",
        "scatter" => "scatter",
        "donut" => "pie",
        "pie" => "pie",
        "treemap" => "treemap",
        "funnel" => "funnel",
        _ => "column",
    };
    let series_data = match visualization_id {
        "scatter" => {
            let points: Vec<Value> = x_values
                .iter()
                .zip(y_values.iter())
                .map(|(x, y)| json!([x, y]))
                .collect();
            json!([{"type": "scatter", "name": "series", "data": points}])
        }
        "donut" | "pie" => {
            let points: Vec<Value> = categories
                .iter()
                .zip(values.iter())
                .map(|(name, value)| json!({"name": name, "y": value, "value": value}))
                .collect();
            json!([{"type": "pie", "name": "series", "data": points}])
        }
        "treemap" | "funnel" => {
            let points: Vec<Value> = categories
                .iter()
                .zip(values.iter())
                .map(|(name, value)| json!({"name": name, "value": value, "y": value}))
                .collect();
            json!([{"type": chart_type, "name": "series", "data": points}])
        }
        "bar100p" => {
            let points: Vec<Value> = categories
                .iter()
                .zip(values.iter())
                .map(|(name, value)| json!({"name": name, "y": value}))
                .collect();
            json!([{"type": "bar", "name": "series", "data": points}])
        }
        _ => {
            let points: Vec<Value> = categories
                .iter()
                .zip(values.iter())
                .map(|(name, value)| json!({"name": name, "y": value}))
                .collect();
            json!([{"type": "bar", "name": "series", "data": points}])
        }
    };
    let mut library = json!({
        "chart": {"type": chart_type},
        "title": {"text": ""},
        "xAxis": {"categories": categories},
        "yAxis": {"title": {"text": ""}},
        "series": series_data,
    });
    if visualization_id == "bar100p" {
        library["plotOptions"] = json!({"bar": {"stacking": "percent"}});
    }
    library
}

/// Last-resort but valid classic Highcharts envelope: one series/one category list built from
/// the fetched rows. Used only when even the classic conversion cannot finish; never empty.
fn minimal_highcharts_finished(
    prepared: &crate::graph_engine::GraphPrepared,
    fetched: &[crate::graph_engine::GraphFetched],
    classic_id: &str,
) -> crate::graph_engine::GraphFinished {
    let response = fetched.first().map(|fetched| fetched.body.clone()).unwrap_or(Value::Null);
    let fields = response
        .get("fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let legend = rows
        .first()
        .and_then(|row| row.get("legend"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let index_by_legend: std::collections::HashMap<i64, usize> = legend
        .iter()
        .enumerate()
        .filter_map(|(index, id)| id.as_i64().map(|id| (id, index)))
        .collect();
    let mut measure_index = None;
    let mut category_index = None;
    for field in &fields {
        let Some(legend_id) = field.get("legend_item_id").and_then(Value::as_i64) else {
            continue;
        };
        let Some(index) = index_by_legend.get(&legend_id).copied() else {
            continue;
        };
        let data_type = field.get("data_type").and_then(Value::as_str).unwrap_or("");
        if measure_index.is_none()
            && matches!(data_type, "integer" | "uinteger" | "float")
        {
            measure_index = Some(index);
        }
        if category_index.is_none() {
            category_index = Some(index);
        }
    }
    let mut categories: Vec<Value> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    for row in &rows {
        let data = row.get("data").and_then(Value::as_array);
        let Some(data) = data else { continue };
        let category = category_index
            .and_then(|index| data.get(index))
            .map(|value| match value {
                Value::String(text) => json!(text),
                other => json!(other.to_string()),
            })
            .unwrap_or_else(|| json!(""));
        categories.push(category);
        let value = measure_index
            .and_then(|index| data.get(index))
            .cloned()
            .unwrap_or(Value::Null);
        values.push(value);
    }
    let chart_type = if classic_id == "pie" { "pie" } else { "column" };
    let library = json!({
        "chart": {"type": chart_type},
        "title": {"text": ""},
        "xAxis": {"categories": categories},
        "yAxis": {"title": {"text": ""}},
        "series": [{"name": "series", "data": values}],
    });
    let data = json!({"categories": categories.clone(), "graphs": []});
    let mut sources = serde_json::Map::new();
    for (request, response) in prepared.requests.iter().zip(fetched.iter()) {
        sources.insert(
            request.key.clone(),
            json!({
                "sourceId": request.key,
                "sourceType": "bi_datasets",
                "status": response.status,
                "latency": response.latency,
                "size": response.size,
                "datasetId": request.dataset,
                "data": request.body,
            }),
        );
    }
    let config = json!({
        "hideHolidaysBands": true,
        "linesLimit": 100,
        "tooltip": {"pin": {"altKey": true}, "sort": {"enabled": true}},
        "preventDefaultForPointClick": false,
        "comments": {"matchType": "intersection", "matchedParams": []},
        "manageTooltipConfig": "wizard-manage-tooltip-config",
    });
    crate::graph_engine::GraphFinished {
        data: crate::graph_engine::GraphData::Value(data),
        config,
        library,
        sources: Value::Object(sources),
    }
}

/// `DL_D3_EMPTY_HC`: d3/gravity clients render from `data`/`config` and ignore
/// `highchartsConfig` (`chart-editor-module.ts:134 buildD3Config: () => {}`). On by
/// default; `DL_D3_EMPTY_HC=0` restores the synthesized classic config.
/// Experimental switch for the gravity line renderer (default off until the full split/axis
/// matrix matches the reference fixtures; the final feature-parity task flips it to default-on).
fn gravity_line_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GRAVITY_LINE")
            .map(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true)
    })
}

/// Experimental switch for the gravity combined-chart renderer (see `gravity_line_enabled`).
fn gravity_combined_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GRAVITY_COMBINED")
            .map(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true)
    })
}

/// Experimental switch for the gravity bar-x renderer (see `gravity_line_enabled`).
fn gravity_barx_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GRAVITY_BARX")
            .map(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true)
    })
}

/// Experimental switch for the gravity area renderer (see `gravity_line_enabled`).
fn gravity_area_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GRAVITY_AREA")
            .map(|value| matches!(value.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(true)
    })
}

fn d3_empty_highcharts() -> bool {
    static EMPTY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *EMPTY.get_or_init(|| {
        !matches!(
            std::env::var("DL_D3_EMPTY_HC").ok().as_deref(),
            Some("0") | Some("false")
        )
    })
}

fn funnel_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// `prepareFunnel` (`preparers/funnel/index.ts`): one funnel series from the measures
/// placeholder, labelled by the first label field and coloured by the first colour field.
fn funnel_finished(
    prepared: &crate::graph_engine::GraphPrepared,
    fetched: &[crate::graph_engine::GraphFetched],
) -> Result<crate::graph_engine::GraphFinished, String> {
    let response = fetched
        .first()
        .ok_or_else(|| "funnel: no fetched source".to_string())?
        .body
        .clone();
    let fields = response
        .get("fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let legend = rows
        .first()
        .and_then(|row| row.get("legend"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let index_by_legend: std::collections::HashMap<i64, usize> = legend
        .iter()
        .enumerate()
        .filter_map(|(index, id)| id.as_i64().map(|id| (id, index)))
        .collect();
    let index_by_guid: std::collections::HashMap<String, usize> = fields
        .iter()
        .filter_map(|field| {
            let id = field.get("id").and_then(Value::as_str)?.to_string();
            let legend_id = field.get("legend_item_id").and_then(Value::as_i64)?;
            index_by_legend.get(&legend_id).map(|index| (id, *index))
        })
        .collect();
    let visualization = prepared.config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let placeholders = visualization
        .get("placeholders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let measures: Vec<Value> = placeholders
        .iter()
        .find(|placeholder| {
            placeholder.get("id").and_then(Value::as_str) == Some("measures")
        })
        .and_then(|placeholder| placeholder.get("items").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let color_field = prepared
        .config
        .get("colors")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned();
    let label_field = prepared
        .config
        .get("labels")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned();
    let index_of = |field: &Value| -> Option<usize> {
        field
            .get("guid")
            .and_then(Value::as_str)
            .and_then(|guid| index_by_guid.get(guid).copied())
    };
    let color_index = color_field.as_ref().and_then(index_of);
    let label_index = label_field.as_ref().and_then(index_of);
    let color_is_measure = color_field
        .as_ref()
        .map(|field| field.get("type").and_then(Value::as_str) == Some("PSEUDO"))
        .unwrap_or(false);
    let mut series_data: Vec<Value> = Vec::new();
    for row in &rows {
        let Some(values) = row.get("data").and_then(Value::as_array) else {
            continue;
        };
        for measure in &measures {
            let Some(index) = index_of(measure) else { continue };
            let Some(raw) = values.get(index) else { continue };
            if raw.is_null() {
                continue;
            }
            let Some(measure_value) = funnel_number(raw) else { continue };
            let name = measure
                .get("fakeTitle")
                .and_then(Value::as_str)
                .or_else(|| measure.get("title").and_then(Value::as_str))
                .unwrap_or("");
            let label = match (&label_field, label_index) {
                (Some(field), _) if field.get("type").and_then(Value::as_str) == Some("PSEUDO") => {
                    json!(name)
                }
                (Some(_), Some(label_index)) => values
                    .get(label_index)
                    .cloned()
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            };
            let color_value = if color_is_measure {
                json!(measure_value)
            } else {
                color_index
                    .and_then(|index| values.get(index))
                    .and_then(funnel_number)
                    .map(|value| json!(value))
                    .unwrap_or(Value::Null)
            };
            series_data.push(json!({
                "value": measure_value,
                "name": name,
                "label": label,
                "custom": {"colorValue": color_value},
            }));
        }
    }
    let data = json!({
        "tooltip": {"enabled": true},
        "legend": {"enabled": series_data.len() > 1},
        "series": {
            "data": [{
                "type": "funnel",
                "name": "",
                "data": series_data,
                "dataLabels": {"enabled": label_field.is_some()},
            }]
        },
        "chart": {"zoom": {"enabled": false}, "margin": {"left": 20, "right": 20}},
    });
    let config = json!({
        "hideHolidaysBands": true,
        "linesLimit": 100,
        "tooltip": {"pin": {"altKey": true}, "sort": {"enabled": true}},
        "preventDefaultForPointClick": false,
        "comments": {"matchType": "intersection", "matchedParams": []},
    });
    let mut sources = serde_json::Map::new();
    for (request, response) in prepared.requests.iter().zip(fetched.iter()) {
        let query = blocks_query(&response.body);
        let mut source = json!({
            "sourceId": request.key,
            "sourceType": "bi_datasets",
            "status": response.status,
            "latency": response.latency,
            "size": response.size,
            "datasetId": request.dataset,
            "data": request.body,
        });
        if !query.is_empty() {
            source["info"] = json!({"query": query});
        }
        sources.insert(request.key.clone(), source);
    }
    Ok(crate::graph_engine::GraphFinished {
        data: crate::graph_engine::GraphData::Value(data),
        config,
        library: json!({}),
        sources: Value::Object(sources),
    })
}

/// Convert a gravity visualization that the dedicated builder could not finish to a classic
/// chartkit visualization (bar→column, bar100p→column100p, scatter→line, donut/pie→pie,
/// tree/funnel→column) and build the normal classic `highchartsConfig` for it. The last
/// resort inside keeps the envelope valid (non-empty HC) even if the conversion itself fails.
fn gravity_classic_fallback(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
    fetched: &[crate::graph_engine::GraphFetched],
    visualization_id: &str,
    default_palette_id: &str,
) -> crate::graph_engine::GraphFinished {
    let classic_id = classic_gravity_id(visualization_id).unwrap_or("column");
    let converted = classic_gravity_shared(shared, visualization_id, classic_id);
    match crate::graph_engine::prepare(&converted, params, datasets) {
        Ok(prepared) => match crate::graph_engine::finish(&prepared, fetched, default_palette_id) {
            Ok(finished) => finished,
            Err(message) => {
                tracing::warn!(%message, visualization = visualization_id, "classic conversion finish failed; minimal Highcharts");
                minimal_highcharts_finished(&prepared, fetched, classic_id)
            }
        },
        Err(message) => {
            tracing::warn!(%message, visualization = visualization_id, "classic conversion prepare failed; minimal Highcharts");
            let empty = crate::graph_engine::GraphPrepared {
                config: converted,
                params: params.clone(),
                requests: Vec::new(),
                drill_down: Value::Null,
            };
            minimal_highcharts_finished(&empty, &[], classic_id)
        }
    }
}

/// Geo (`ymap_wizard_node`) path: the builder lives in `geo_engine`; this wrapper only
/// supplies the shared envelope (`GraphEnvelope`) exactly like the graph/metric paths.
async fn run_rust_geo(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    meta_entry_id: Value,
    meta_key: Value,
    meta_rev: Value,
    response_id: &str,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let answer = crate::geo_engine::run(state, shared, params, &scope.headers).await?;
    // Geo keeps its plain `Value` data; the shared graph envelope takes the typed wrapper.
    let geo_data = crate::graph_engine::GraphData::Value(answer.data);
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let config_text = serde_json::to_string(&answer.config)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let highcharts_text = serde_json::to_string(&answer.library)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let (params, used_params) = chart_params(
        shared,
        params,
        &[answer.dataset_schema.as_array().map(Vec::as_slice).unwrap_or(&[])],
    );
    let geo_dataset = shared_dataset_ids(shared).into_iter().next().unwrap_or_default();
    let extra = chart_extra(
        state,
        shared,
        &json!([{"id": geo_dataset, "fields": answer.dataset_schema}]),
        answer.forbidden,
    );
    let conf = json!({
        "type": "ymap_wizard_node",
        "meta": {"stype": "ymap_wizard_node"},
        "entryId": meta_entry_id,
        "key": meta_key,
        "revId": meta_rev,
    });
    let envelope = GraphEnvelope {
        sources: &answer.sources,
        ui_scheme: None,
        params: &params,
        used_params: &used_params,
        action_params: EmptyObject {},
        widget_config: EmptyObject {},
        default_params: EmptyObject {},
        extra,
        timings: MetricTimings {
            config_resolving: (elapsed - answer.latency_ms as f64).max(0.0),
            data_fetching: answer.latency_ms as f64,
            js_execution: 0.0,
        },
        data: &geo_data,
        config: &config_text,
        highcharts_config: &highcharts_text,
        data_export: &answer.data_export,
        conf_storage_config: &conf,
        key: &meta_key,
        id: response_id,
        kind: "ymap_wizard_node",
        rev_id: &meta_rev,
    };
    let mut buffer = BytesMut::with_capacity(16 * 1024);
    {
        let mut writer = (&mut buffer).writer();
        serde_json::to_writer(&mut writer, &envelope)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    }
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(response_body(buffer))
        .expect("geo response"))
}

async fn run_rust_graph(
    state: &Arc<AppState>, shared: &Value, params: &Value, _entry_id: &str,
    meta_entry_id: Value, meta_key: Value, meta_rev: Value, response_id: &str,
    started: std::time::Instant, scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let renderer = graph_renderer(shared);
    let visualization_id = shared
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(renderer, GraphRenderer::Chartkit)
        && !(renderer == GraphRenderer::Gravity
            && matches!(
                visualization_id,
                "bar" | "bar100p" | "scatter" | "donut" | "pie" | "treemap" | "funnel"
            ))
    {
        return Err((StatusCode::NOT_IMPLEMENTED, "graph: renderer is not ported to Rust".to_string()));
    }
    let fields_started = std::time::Instant::now();
    let datasets_ids = shared_dataset_ids(shared);
    let mut datasets = std::collections::HashMap::new();
    for dataset in &datasets_ids {
        let (fields, revision) = dataset_fields(state, scope, dataset).await?;
        datasets.insert(dataset.clone(), crate::graph_engine::DatasetInfo { fields, revision });
    }
    record_stage("graph.fields", fields_started.elapsed().as_secs_f64() * 1000.0);
    let mut prepare_used_classic = false;
    let prepared = if visualization_id == "funnel" {
        // A funnel is a gravity chart with its own series shape, not a `treemap`: the
        // treemap builder rejects it and the classic conversion turned it into a column.
        crate::graph_engine::prepare(shared, params, &datasets)
            .map_err(|error| (StatusCode::NOT_IMPLEMENTED, error))?
    } else if let Some(classic_id) = classic_gravity_id(visualization_id) {
        let attempted = match visualization_id {
            "bar" | "bar100p" => crate::gravity_bar::prepare(shared, params, &datasets),
            "scatter" => crate::gravity_scatter::prepare(shared, params, &datasets),
            "donut" | "pie" => crate::gravity_donut::prepare(shared, params, &datasets),
            "treemap" => crate::gravity_tree::prepare(shared, params, &datasets),
            _ => crate::graph_engine::prepare(shared, params, &datasets),
        };
        match attempted {
            Ok(prepared) => prepared,
            Err(message) => {
                tracing::warn!(%message, visualization = visualization_id, "gravity builder -> classic Highcharts conversion");
                let converted = classic_gravity_shared(shared, visualization_id, classic_id);
                prepare_used_classic = true;
                crate::graph_engine::prepare(&converted, params, &datasets)
                    .map_err(|error| (StatusCode::NOT_IMPLEMENTED, error))?
            }
        }
    } else {
        let attempted = match visualization_id {
            "area" if crate::d3_area::handles(shared) => {
                crate::d3_area::prepare(shared, params, &datasets)
            }
            _ => crate::graph_engine::prepare(shared, params, &datasets),
        };
        match attempted {
            Ok(prepared) => prepared,
            Err(message) => return Err((StatusCode::NOT_IMPLEMENTED, message)),
        }
    };
    let fetch_started = std::time::Instant::now();
    let mut fetched = Vec::with_capacity(prepared.requests.len());
    // The UI expects `dataExport` to be a map source-id -> {background, basic}
    // (it does Object.values(dataExport) and reads value.basic.allowed). The graph
    // path used to emit the flat {background, basic} block, which made the export
    // menu throw "Cannot read properties of undefined (reading 'allowed')".
    // Key by the same source id the graph sources use, so one entry per data source.
    let mut data_export = serde_json::Map::new();
    for request in &prepared.requests {
        let request_started = std::time::Instant::now();
        // v8.2: typed in-process data-api (embedded track) → rebuild the wire `Value` the
        // graph builders consume by moving the typed rows/fields/cells, and take `size` from
        // the typed serialization instead of walking the rebuilt tree. The HTTP fallback is
        // unchanged.
        let (status, body, size, export) = if crate::data_direct::installed() {
            match crate::data_direct::result_typed(
                &state.http,
                &state.data_api,
                &scope.headers,
                &request.dataset,
                &request.body,
            )
            .await
            .map_err(|(status, message)| (status, format!("graph data-api: {message}")))?
            {
                crate::data_direct::TypedResult::Ok(outcome) => {
                    let status = outcome.status;
                    let size = outcome.json_len;
                    let data_export = outcome.data_export;
                    let Some((body, export)) = outcome.body.into_result_wire_value(data_export) else {
                        return Err((
                            StatusCode::BAD_GATEWAY,
                            "graph data-api: unexpected pivot body".to_string(),
                        ));
                    };
                    (status, body, size, Some(export))
                }
                crate::data_direct::TypedResult::Failed { status, body } => {
                    if !status.is_success() {
                        return Err(data_fetch_error(
                            &request.key,
                            status.as_u16(),
                            &request.body,
                            &body,
                        ));
                    }
                    let size = json_len(&body) as u64;
                    let export = body.get("data_export").cloned();
                    (status, body, size, export)
                }
            }
        } else {
            let outcome = crate::data_direct::result(
                &state.http,
                &state.data_api,
                &scope.headers,
                &request.dataset,
                &request.body,
            )
            .await
            .map_err(|(status, message)| (status, format!("graph data-api: {message}")))?;
            let status = outcome.status;
            let size = outcome.json_len;
            let export = outcome.data_export;
            let body = outcome.body;
            if !status.is_success() {
                return Err(data_fetch_error(
                    &request.key,
                    status.as_u16(),
                    &request.body,
                    &body,
                ));
            }
            (status, body, size, export)
        };
        if let Some(export) = export {
            data_export.insert(request.key.clone(), export);
        }
        fetched.push(crate::graph_engine::GraphFetched {
            key: request.key.clone(),
            dataset: request.dataset.clone(),
            status: status.as_u16(),
            latency: request_started.elapsed().as_secs_f64() * 1000.0,
            size,
            body,
            data_export: None,
        });
    }
    let fetch_ended = std::time::Instant::now();
    let latency = fetch_ended.duration_since(fetch_started).as_millis() as u64;
    record_stage("graph.fetch", latency as f64);
    // `js.ts:487-680`: rows/segments limits checked on the raw data-api rows.
    let rows_limit = graph_rows_limit(visualization_id);
    let fetched_rows = fetched.iter().map(|item| result_rows(&item.body)).max().unwrap_or(0);
    if fetched_rows > rows_limit {
        return Err(rows_oversize(rows_limit, fetched_rows));
    }
    if let Some(first) = fetched.first() {
        let segments = segments_number(&prepared.config, &first.body);
        if segments > 25 {
            return Err(segments_oversize(25, segments));
        }
    }
    let finish_started = std::time::Instant::now();
    let mut used_classic = prepare_used_classic;
    let mut finished: crate::graph_engine::GraphFinished = if prepare_used_classic {
        match crate::graph_engine::finish(&prepared, &fetched, &state.default_color_palette) {
            Ok(finished) => finished,
            Err(message) => {
                tracing::warn!(%message, visualization = visualization_id, "classic gravity finish failed; minimal Highcharts");
                minimal_highcharts_finished(&prepared, &fetched, classic_gravity_id(visualization_id).unwrap_or("column"))
            }
        }
    } else {
        match visualization_id {
            "bar" | "bar100p" => match crate::gravity_bar::finish(&prepared, &fetched, &state.default_color_palette) {
                Ok(finished) => crate::graph_engine::GraphFinished {
                    data: crate::graph_engine::GraphData::Value(finished.data),
                    config: finished.config,
                    library: finished.library,
                    sources: finished.sources,
                },
                Err(message) => {
                    tracing::warn!(%message, "gravity bar fallback");
                    used_classic = true;
                    gravity_classic_fallback(shared, params, &datasets, &fetched, visualization_id, &state.default_color_palette)
                }
            },
            "scatter" => match crate::gravity_scatter::finish(&prepared, &fetched, &state.default_color_palette) {
                Ok(finished) => finished,
                Err(message) => {
                    tracing::warn!(%message, "gravity scatter fallback");
                    used_classic = true;
                    gravity_classic_fallback(shared, params, &datasets, &fetched, visualization_id, &state.default_color_palette)
                }
            },
            "funnel" => funnel_finished(&prepared, &fetched)
                .map_err(|message| (StatusCode::NOT_IMPLEMENTED, message))?,
            "treemap" => match crate::gravity_tree::finish(&prepared, &fetched, &state.default_color_palette) {
                Ok(finished) => finished,
                Err(message) => {
                    tracing::warn!(%message, "gravity tree fallback");
                    used_classic = true;
                    gravity_classic_fallback(shared, params, &datasets, &fetched, visualization_id, &state.default_color_palette)
                }
            },
            "donut" | "pie" => match crate::gravity_donut::finish(&prepared, &fetched, &state.default_color_palette) {
                Ok(finished) => finished,
                Err(message) => {
                    tracing::warn!(%message, "gravity donut fallback");
                    used_classic = true;
                    gravity_classic_fallback(shared, params, &datasets, &fetched, visualization_id, &state.default_color_palette)
                }
            },
            "area" if crate::d3_area::handles(shared) => {
                match crate::d3_area::finish(&prepared, &fetched, &state.default_color_palette) {
                    Ok(finished) => finished,
                    Err(message) => return Err((StatusCode::NOT_IMPLEMENTED, message)),
                }
            }
            _ => match crate::graph_engine::finish(&prepared, &fetched, &state.default_color_palette) {
                Ok(finished) => finished,
                Err(message) => return Err((StatusCode::NOT_IMPLEMENTED, message)),
            },
        }
    };
    // `ChartEditor.setDataSourceInfo(key, {query})` applies to every renderer; the gravity
    // builders build `sources` themselves, so fill the SQL inspector here for them too.
    if let Some(sources) = finished.sources.as_object_mut() {
        for (key, source) in sources.iter_mut() {
            if source.get("info").is_some() {
                continue;
            }
            let query = fetched
                .iter()
                .find(|item| &item.key == key)
                .map(|item| blocks_query(&item.body))
                .unwrap_or_default();
            if !query.is_empty() {
                source["info"] = json!({"query": query});
            }
        }
    }
    let gravity_line_used = visualization_id == "line" && gravity_line_enabled();
    let gravity_area_used =
        matches!(visualization_id, "area" | "area100p") && gravity_area_enabled();
    let gravity_barx_used =
        matches!(visualization_id, "column" | "column100p") && gravity_barx_enabled();
    let gravity_combined_used =
        visualization_id == "combined-chart" && gravity_combined_enabled();
    if gravity_line_used || gravity_area_used || gravity_barx_used || gravity_combined_used {
        let mut response_titles: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for item in &fetched {
            for field in item.body.get("fields").and_then(Value::as_array).into_iter().flatten() {
                if let (Some(id), Some(title)) = (
                    field.get("id").and_then(Value::as_str),
                    field.get("title").and_then(Value::as_str),
                ) {
                    response_titles.entry(id.to_string()).or_insert_with(|| title.to_string());
                }
            }
        }
        if let crate::graph_engine::GraphData::Value(classic) = &finished.data {
            let converted = if gravity_area_used {
                crate::gravity_line::convert_area(shared, classic, visualization_id, &response_titles)
            } else if gravity_barx_used {
                crate::gravity_line::convert_bar_x(shared, classic, visualization_id, &response_titles)
            } else if gravity_combined_used {
                crate::gravity_line::convert_combined(shared, classic, &response_titles)
            } else {
                crate::gravity_line::convert(shared, classic, &response_titles)
            }
            .map_err(|message| (StatusCode::INTERNAL_SERVER_ERROR, message))?;
            finished.data = crate::graph_engine::GraphData::Value(converted);
        }
        // The gravity `config` member is `buildChartsConfigPrivate` without the Highcharts-only
        // `precision`/`highstock` keys the classic config carries.
        if let Some(config) = finished.config.as_object_mut() {
            config.remove("precision");
            config.remove("highstock");
        }
    }
    if !used_classic
        && !d3_empty_highcharts()
        && matches!(
            visualization_id,
            "bar" | "bar100p" | "scatter" | "donut" | "pie" | "treemap" | "funnel"
        )
    {
        // Gravity charts are rendered client-side from `data`/`config`, but the envelope
        // contract requires a non-empty `highchartsConfig`; derive a valid classic one from
        // the fetched rows (chart type matches the visualization).
        let empty = Value::Null;
        let response: &Value = fetched
            .first()
            .map(|fetched| &fetched.body)
            .unwrap_or(&empty);
        finished.library = gravity_highcharts_library(visualization_id, response);
    }
    record_stage("graph.finish", finish_started.elapsed().as_secs_f64() * 1000.0);
    let response_type = if used_classic {
        "graph_wizard_node"
    } else if gravity_line_used
        || gravity_area_used
        || gravity_barx_used
        || gravity_combined_used
        || matches!(visualization_id, "bar" | "bar100p" | "scatter" | "donut" | "pie" | "treemap" | "funnel")
    {
        "d3_wizard_node"
    } else {
        "graph_wizard_node"
    };
    let conf = json!({"type":response_type,"meta":{"stype":response_type},"entryId":meta_entry_id,"key":meta_key,"revId":meta_rev});
    let data_export = Value::Object(data_export);
    let reply_started = std::time::Instant::now();
    let response_field_sets: Vec<&[Value]> = fetched
        .iter()
        .filter_map(|item| item.body.get("fields").and_then(Value::as_array).map(Vec::as_slice))
        .collect();
    let (params, used_params) = chart_params(shared, &prepared.params, &response_field_sets);
    let forbidden = fetched
        .iter()
        .any(|item| item.body.get("data_export_forbidden") == Some(&Value::Bool(true)));
    let extra_datasets: Vec<Value> = fetched
        .iter()
        .map(|item| {
            json!({
                "id": item.dataset,
                "fields": item.body.get("fields").cloned().unwrap_or_else(|| json!([])),
            })
        })
        .collect();
    let extra = chart_extra(state, shared, &Value::Array(extra_datasets), forbidden);
    let completed = std::time::Instant::now();
    let timings = MetricTimings::from_native_stages(started, fetch_started, fetch_ended, completed);
    // C2: the response is written into one buffer. `config`/`highchartsConfig` are streamed as
    // escaped JSON text straight from the typed values — no intermediate `String` copies — and
    // `data` is serialized from `GraphData` without building an envelope `Value` tree.
    let io_error = |error: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
    let mut envelope = crate::raw_envelope::Envelope::with_capacity(32 * 1024);
    envelope.member_value("sources", &finished.sources).map_err(io_error)?;
    envelope.member_raw("uiScheme", "null").map_err(io_error)?;
    envelope.member_value("params", &params).map_err(io_error)?;
    envelope.member_value("usedParams", &used_params).map_err(io_error)?;
    envelope.member_raw("actionParams", "{}").map_err(io_error)?;
    envelope.member_raw("widgetConfig", "{}").map_err(io_error)?;
    envelope.member_raw("defaultParams", "{}").map_err(io_error)?;
    envelope.member_value("extra", &extra).map_err(io_error)?;
    envelope.member_serialize("timings", &timings).map_err(io_error)?;
    envelope.member_serialize("data", &finished.data).map_err(io_error)?;
    envelope
        .member_json_text_serialize("config", &finished.config)
        .map_err(io_error)?;
    if response_type == "d3_wizard_node" && d3_empty_highcharts() {
        envelope.member_json_literal("highchartsConfig", "{}").map_err(io_error)?;
    } else {
        envelope
            .member_json_text_serialize("highchartsConfig", &finished.library)
            .map_err(io_error)?;
    }
    envelope.member_value("dataExport", &data_export).map_err(io_error)?;
    envelope.member_value("_confStorageConfig", &conf).map_err(io_error)?;
    envelope.member_value("key", &meta_key).map_err(io_error)?;
    envelope.member_str("id", response_id).map_err(io_error)?;
    envelope.member_str("type", response_type).map_err(io_error)?;
    envelope.member_value("revId", &meta_rev).map_err(io_error)?;
    record_stage("graph.reply", reply_started.elapsed().as_secs_f64() * 1000.0);
    envelope.finish()
}

/// `sources.<id>` entry of the metric envelope. Serialized straight from borrowed
/// values: the old `json!` envelope went through `to_value`, deep-copying the payload.
#[derive(serde::Serialize)]
struct MetricSource<'a> {
    #[serde(rename = "sourceId")]
    source_id: &'a str,
    #[serde(rename = "sourceType")]
    source_type: &'static str,
    status: u16,
    latency: f64,
    size: usize,
    #[serde(rename = "datasetId")]
    dataset_id: &'a str,
    data: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<Value>,
}

#[derive(serde::Serialize)]
struct MetricTimings {
    #[serde(rename = "configResolving")]
    config_resolving: f64,
    #[serde(rename = "dataFetching")]
    data_fetching: f64,
    #[serde(rename = "jsExecution")]
    js_execution: f64,
}

impl MetricTimings {
    /// Split the native request into the inspector's three stages. `dataFetching` is the
    /// actual data-api call; native chart/config work after it is reported as `jsExecution`
    /// (the Rust replacement for the Python JS processor), not folded into data fetching.
    fn from_native_stages(
        started: std::time::Instant,
        fetch_started: std::time::Instant,
        fetch_ended: std::time::Instant,
        completed: std::time::Instant,
    ) -> Self {
        let config_resolving = fetch_started.duration_since(started).as_secs_f64() * 1000.0;
        let data_fetching = fetch_ended.duration_since(fetch_started).as_secs_f64() * 1000.0;
        let js_execution = (completed.duration_since(started).as_secs_f64() * 1000.0
            - config_resolving
            - data_fetching)
            .max(0.0);
        Self {
            config_resolving: config_resolving.max(0.0),
            data_fetching: data_fetching.max(0.0),
            js_execution,
        }
    }
}

#[derive(serde::Serialize)]
struct EmptyObject {}

/// The `/api/run` answer for a metric chart, serialized in one pass.
#[derive(serde::Serialize)]
struct MetricEnvelope<'a> {
    sources: std::collections::BTreeMap<&'a str, MetricSource<'a>>,
    #[serde(rename = "uiScheme")]
    ui_scheme: Option<()>,
    params: &'a Value,
    #[serde(rename = "usedParams")]
    used_params: &'a Value,
    #[serde(rename = "actionParams")]
    action_params: EmptyObject,
    #[serde(rename = "widgetConfig")]
    widget_config: EmptyObject,
    #[serde(rename = "defaultParams")]
    default_params: EmptyObject,
    extra: Value,
    timings: MetricTimings,
    data: &'a Value,
    config: &'static str,
    #[serde(rename = "highchartsConfig")]
    highcharts_config: &'static str,
    #[serde(rename = "dataExport")]
    data_export: std::collections::BTreeMap<&'a str, &'a Value>,
    #[serde(rename = "_confStorageConfig")]
    conf_storage_config: &'a Value,
    key: &'a Value,
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(rename = "revId")]
    rev_id: &'a Value,
}

/// Stand-in for an absent `shared.extraSettings`: `.get(...)` on `Null` returns the same
/// `None` the old `cloned().unwrap_or_else(json!({}))` fallback produced, without allocating.
static METRIC_MISSING_SETTINGS: Value = Value::Null;

/// `prepareMetric`'s single data point, shared by the typed-body path (v8.1) and the JSON
/// fallback so both produce exactly the same envelope `data`.
fn metric_data_point(
    raw_value: Option<&Value>,
    formatting: Option<&serde_json::Map<String, Value>>,
    data_type: &str,
    size: &str,
    color: &str,
    title: &str,
) -> Value {
    let Some(raw_value) = raw_value else {
        // `prepareMetric` returns `[]` for a null/undefined value.
        return json!([]);
    };
    if raw_value.is_null() {
        return json!([]);
    }
    let value = match raw_value {
        Value::Number(number) => Value::Number(number.clone()),
        Value::String(text) => text
            .parse::<f64>()
            .map(metric_json_number)
            .unwrap_or(Value::Null),
        other => other.clone(),
    };
    let mut current = serde_json::Map::with_capacity(6);
    current.insert("value".to_string(), value);
    for key in ["format", "postfix", "prefix", "showRankDelimiter", "unit"] {
        if let Some(value) = formatting.and_then(|formatting| formatting.get(key)) {
            current.insert(key.to_string(), value.clone());
        }
    }
    if let Some(precision) = formatting
        .and_then(|formatting| formatting.get("precision"))
        .filter(|value| value.is_number())
    {
        current.insert("precision".to_string(), precision.clone());
    } else if data_type == "float" {
        current.insert("precision".to_string(), json!(2));
    }
    json!([{
        "content": {"current": Value::Object(current)},
        "size": size,
        "color": color,
        "title": title,
    }])
}

/// Typed-body variant of the metric data extraction (v8.1): reads the typed `ResultBody`
/// rows/fields instead of a `serde_json::Value` tree. Output is identical to the JSON branch
/// in `run_metric_rust` (unit-tested against `metric_data_point`).
fn metric_typed_data_point(
    body: &crate::typed_data::DataBody,
    guid: &str,
    formatting: Option<&serde_json::Map<String, Value>>,
    data_type: &str,
    size: &str,
    color: &str,
    title: &str,
) -> Value {
    let Some((result, row)) = body
        .as_result()
        .and_then(|result| result.rows().first().map(|row| (result, row)))
    else {
        // `prepareSingleResult` returns `{}` when there are no rows.
        return json!({});
    };
    let measure_index = row
        .legend
        .iter()
        .position(|legend_id| {
            result.fields().iter().any(|field| {
                field.legend_item_id == *legend_id && field.id == guid
            })
        })
        .unwrap_or(0);
    metric_data_point(
        row.data.get(measure_index),
        formatting,
        data_type,
        size,
        color,
        title,
    )
}

/// Rust fast path for `metric_wizard_node`. It builds the data-api request from the metric
/// visualization directly; no V8 is involved for the chart itself.
async fn run_metric_rust(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    _lang: &str,
    _entry_id: &str,
    meta_entry_id: Value,
    meta_key: Value,
    meta_rev: Value,
    response_id: &str,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let dataset = shared_dataset_ids(shared)
        .into_iter()
        .next()
        .ok_or_else(|| cold_bad_request("metric: dataset is missing"))?;
    let dataset = dataset.as_str();
    let measure = shared
        .pointer("/visualization/placeholders")
        .and_then(|value| value.as_array())
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(|value| value.as_str()) == Some("measures")
            })
        })
        .and_then(|placeholder| placeholder.get("items"))
        .and_then(|items| items.as_array())
        .and_then(|items| items.first())
        .ok_or_else(|| cold_bad_request("metric: measure is missing"))?;
    let guid = measure
        .get("guid")
        .and_then(|value| value.as_str())
        .ok_or_else(|| cold_bad_request("metric: measure guid is missing"))?;
    let data_type = measure
        .get("data_type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !matches!(data_type, "integer" | "uinteger" | "float") {
        // Markup and date metrics are rendered by another V8 preparer variant.
        return Err(cold_bad_request("metric: non-numeric measure"));
    }
    let extra_settings = shared
        .get("extraSettings")
        .unwrap_or(&METRIC_MISSING_SETTINGS);
    if extra_settings.get("overlap").and_then(|value| value.as_str()) == Some("on")
        || extra_settings.get("tooltip").and_then(|value| value.as_str()) == Some("hide")
    {
        // These extra settings change `highchartsConfig` outside the metric defaults.
        return Err(cold_bad_request("metric: unsupported extra settings"));
    }
    // V8 `getTitle` uses `indicatorTitleMode` (v11+); old configs get it from `titleMode` in
    // `mapV10ConfigToV11`. `titleMode: "hide"` maps to `"by-field"`, not to `"hide"`.
    let title_mode = extra_settings
        .get("indicatorTitleMode")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let title = extra_settings
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let manual = extra_settings.get("titleMode").and_then(|value| value.as_str()) == Some("show")
                && !title.is_empty();
            if manual { "manual".to_string() } else { "by-field".to_string() }
        });
    let title = match title_mode.as_str() {
        "hide" => String::new(),
        "manual" => extra_settings
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string(),
        _ => measure
            .get("fakeTitle")
            .and_then(|value| value.as_str())
            .or_else(|| measure.get("title").and_then(|value| value.as_str()))
            .unwrap_or("")
            .to_string(),
    };
    let size = extra_settings
        .get("metricFontSize")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // V8 `prepareBasicMetricVariant` -> `getColorByColorSettings`: `metricFontColorIndex` wins,
    // then the explicit `metricFontColor`, then the first colour of the server palette
    // (`selectServerPalette`, `default20` unless `metricFontColorPalette` overrides it).
    let palette: &'static [Value] = {
        let system = system_palettes();
        let requested = extra_settings
            .get("metricFontColorPalette")
            .and_then(|value| value.as_str())
            .filter(|id| !id.is_empty() && *id != "auto");
        let palette_id = match requested {
            Some(id) if system.get(id).is_some() => id,
            Some(_) => {
                // Custom entry palettes are fetched by the V8 pipeline only.
                return Err(cold_bad_request("metric: custom palette"));
            }
            None => state.default_color_palette.as_str(),
        };
        system
            .get(palette_id)
            .and_then(|value| value.get("scheme"))
            .and_then(|value| value.as_array())
            .map(|items| items.as_slice())
            .filter(|scheme| !scheme.is_empty())
            .ok_or_else(|| cold_bad_request("metric: palette is missing"))?
    };
    let color = match extra_settings
        .get("metricFontColorIndex")
        .and_then(|value| value.as_i64())
    {
        Some(index) if index >= 0 => palette
            .get(index as usize)
            .or_else(|| palette.first())
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_default(),
        _ => extra_settings
            .get("metricFontColor")
            .and_then(|value| value.as_str())
            .filter(|color| !color.is_empty())
            .map(str::to_string)
            .or_else(|| {
                palette
                    .first()
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default(),
    };
    // V8 `getFormatOptions`: a non-empty `formatting` object wins, otherwise
    // `field.ui_settings.numberFormatting` (a JSON string) is used, else `{}`.
    let fallback_formatting = measure
        .get("ui_settings")
        .and_then(|value| value.as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|value| value.get("numberFormatting").cloned());
    // `formatting` is only read: borrow the measure's object, or the parsed
    // `ui_settings.numberFormatting` fallback (kept alive above for the borrow).
    let formatting = measure
        .get("formatting")
        .and_then(|value| value.as_object())
        .filter(|formatting| !formatting.is_empty())
        .or_else(|| {
            fallback_formatting
                .as_ref()
                .and_then(|value| value.as_object())
        });
    // Dashboard date-range params are stored as `__interval_*` markers (both sides may be
    // `__relative_*`); resolve them before they reach the data-api (same helper as the
    // graph/table/pivot paths — otherwise the data-api gets a one-value `BETWEEN`/bad literal).
    let resolved_params = crate::pivot_engine::resolve_params(params)
        .unwrap_or_else(|_| params.clone());
    let params = &resolved_params;
    let mut filters =
        Vec::with_capacity(params.as_object().map(|values| values.len()).unwrap_or(0));
    if let Some(values) = params.as_object() {
        for (key, value) in values {
            let was_array = value.is_array();
            let raw: Vec<Value> = match value {
                Value::Array(items) => items.clone(),
                Value::Null => continue,
                other => vec![other.clone()],
            };
            if raw.is_empty() {
                continue;
            }
            let (mut values, operation) =
                crate::pivot_engine::resolve_filter_values(&raw, "IN");
            // `prepareArrayFilterValues`: an interval -> `BETWEEN`, an array param -> `IN`
            // (even with a single element: the dashboard control sends `["North"]`), a scalar
            // param -> `EQ`.
            let operation = if operation == "IN" && !was_array {
                "EQ".to_string()
            } else {
                operation
            };
            values.retain(|item| !item.is_null() && item.as_str() != Some(""));
            if values.is_empty() {
                continue;
            }
            filters.push(json!({
                "ref": {"type": "id", "id": key},
                "operation": operation,
                "values": values,
            }));
        }
    }
    // Local fields added on the chart (`shared.updates`) are part of the query: without them
    // the data-api answers a coarser row (58 fields instead of 59 for "Charts: Indicator 1").
    let updates: Vec<Value> = shared
        .get("updates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|update| {
            update
                .pointer("/field/datasetId")
                .and_then(Value::as_str)
                .map(|update_dataset| update_dataset == dataset)
                .unwrap_or(true)
        })
        .collect();
    // Pin the dataset revision when the request-scoped US bundle has it (same revision the
    // table/graph/pivot builders take from `/fields`). Without it the data-api cannot pin the
    // dataset and the query-result cache key has no revision to invalidate on (verifier T5:
    // a dataset revision change was served from the cache while this was `null`).
    let dataset_revision = crate::us_direct::bundled_revision(dataset);
    let mut request_body = json!({
        "fields": [{
            "ref": {"type": "id", "id": guid},
            "block_id": 0,
            "role_spec": {"role": "measure"},
        }],
        "filters": filters,
        "order_by": [],
        "parameter_values": [],
        "dataset_revision_id": dataset_revision,
        "ignore_nonexistent_filters": true,
        "with_totals": false,
        "autofill_legend": true,
    });
    if !updates.is_empty() {
        request_body["updates"] = Value::Array(updates);
    }
    let fetch_started = std::time::Instant::now();
    // v8.1: with the typed in-process data-api installed (embedded track) the metric path
    // consumes the typed `ResultResponse` directly: no `serde_json::to_value` body and no
    // second JSON walk to compute `source.size` (the length comes from the same serialization
    // pass, into a bounded per-worker scratch). The HTTP fallback (mock/standalone) keeps the
    // old Value path unchanged.
    enum MetricBody {
        Typed(crate::typed_data::DataBody),
        Json(Value),
    }
    let (status, body_source, body_json_len, data_export_value) =
        if crate::data_direct::installed() {
            match crate::data_direct::result_typed(
                &state.http,
                &state.data_api,
                &scope.headers,
                dataset,
                &request_body,
            )
            .await
            .map_err(|(status, message)| (status, format!("metric data-api: {message}")))?
            {
                crate::data_direct::TypedResult::Ok(outcome) => {
                    let status = outcome.status;
                    if !status.is_success() {
                        let body = outcome.body.to_value().unwrap_or_else(|_| json!({}));
                        return Err(data_fetch_error(
                            &source_key_for(dataset),
                            status.as_u16(),
                            &request_body,
                            &body,
                        ));
                    }
                    (
                        status,
                        MetricBody::Typed(outcome.body),
                        outcome.json_len as usize,
                        outcome.data_export,
                    )
                }
                crate::data_direct::TypedResult::Failed { status, body } => {
                    return Err(data_fetch_error(
                        &source_key_for(dataset),
                        status.as_u16(),
                        &request_body,
                        &body,
                    ));
                }
            }
        } else {
            let outcome = crate::data_direct::result(
                &state.http,
                &state.data_api,
                &scope.headers,
                dataset,
                &request_body,
            )
            .await
            .map_err(|(status, message)| (status, format!("metric data-api: {message}")))?;
            let status = outcome.status;
            let len = outcome.json_len as usize;
            let body = outcome.body;
            if !status.is_success() {
                return Err(data_fetch_error(
                    &source_key_for(dataset),
                    status.as_u16(),
                    &request_body,
                    &body,
                ));
            }
            let export = body
                .get("data_export")
                .cloned()
                .unwrap_or_else(|| json!({}));
            (status, MetricBody::Json(body), len, export)
        };
    let fetch_ended = std::time::Instant::now();
    let data = match &body_source {
        MetricBody::Typed(body) => {
            metric_typed_data_point(body, guid, formatting, data_type, size, &color, &title)
        }
        MetricBody::Json(body) => {
            let rows = body
                .pointer("/result_data/0/rows")
                .and_then(|value| value.as_array());
            match rows {
                Some(rows) if !rows.is_empty() => {
                    let row = &rows[0];
                    let measure_index = row
                        .get("legend")
                        .and_then(|value| value.as_array())
                        .and_then(|legend| {
                            legend.iter().position(|legend_id| {
                                body.get("fields")
                                    .and_then(|value| value.as_array())
                                    .map(|fields| {
                                        fields.iter().any(|field| {
                                            field.get("legend_item_id") == Some(legend_id)
                                                && field.get("id").and_then(|value| value.as_str())
                                                    == Some(guid)
                                        })
                                    })
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(0);
                    let raw_value = row
                        .get("data")
                        .and_then(|value| value.as_array())
                        .and_then(|cells| cells.get(measure_index));
                    metric_data_point(
                        raw_value,
                        formatting,
                        data_type,
                        size,
                        &color,
                        &title,
                    )
                }
                // `prepareSingleResult` returns `{}` when there are no rows.
                _ => json!({}),
            }
        }
    };
    let rows_count = match &body_source {
        MetricBody::Typed(body) => body
            .as_result()
            .map(|result| result.rows().len())
            .unwrap_or(0),
        MetricBody::Json(body) => result_rows(body),
    };
    // `js.ts:566`: the metric preparer reads only the first row, but the limit is the
    // same ROWS_NUMBER_OVERSIZE contract as the other 1000-row visualizations.
    if rows_count > 1_000 {
        return Err(rows_oversize(1_000, rows_count));
    }
    let (query, forbidden) = match &body_source {
        MetricBody::Typed(body) => (
            typed_blocks_query(body),
            body.as_result()
                .map(|result| result.data_export_forbidden())
                .unwrap_or(false),
        ),
        MetricBody::Json(body) => (
            blocks_query(body),
            body.get("data_export_forbidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    };
    let schema = shared
        .get("datasetsPartialFields")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or_else(|| json!([]));
    let metric_fields = match &body_source {
        MetricBody::Typed(body) => body
            .as_result()
            .and_then(|result| serde_json::to_value(&result.response.fields).ok())
            .unwrap_or_else(|| json!([])),
        MetricBody::Json(body) => body.get("fields").cloned().unwrap_or_else(|| json!([])),
    };
    let metric_fields_slice = metric_fields.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let schema_slice = schema.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let (params, used_params) = chart_params(
        shared,
        params,
        &[metric_fields_slice, schema_slice],
    );
    let extra = chart_extra(
        state,
        shared,
        &json!([{"id": dataset, "fields": metric_fields}]),
        forbidden,
    );
    let config = r#"{"hideHolidaysBands":true,"linesLimit":100,"tooltip":{"pin":{"altKey":true},"sort":{"enabled":true}},"preventDefaultForPointClick":false,"comments":{"matchType":"intersection","matchedParams":[]},"metricVersion":2}"#;
    // `buildHighchartsConfigPrivate` starts with `legend = {}` and only turns it into
    // `{"enabled":false}` when `extraSettings.legendMode === "hide"`. Only two variants
    // exist, so both are compile-time strings instead of a per-request `concat`.
    let highcharts: &'static str = if extra_settings
        .get("legendMode")
        .and_then(|value| value.as_str())
        == Some("hide")
    {
        r#"{"chart":{"type":"metric","zoomType":"x"},"legend":{"enabled":false},"xAxis":{"endOnTick":false},"yAxis":{},"tooltip":{},"plotOptions":{"series":{"dataGrouping":{"enabled":false},"dataLabels":{"allowOverlap":false}}},"navigator":{"series":{"dataLabels":{"color":"transparent"},"fillOpacity":0.15}}}"#
    } else {
        r#"{"chart":{"type":"metric","zoomType":"x"},"legend":{},"xAxis":{"endOnTick":false},"yAxis":{},"tooltip":{},"plotOptions":{"series":{"dataGrouping":{"enabled":false},"dataLabels":{"allowOverlap":false}}},"navigator":{"series":{"dataLabels":{"color":"transparent"},"fillOpacity":0.15}}}"#
    };
    let latency = fetch_ended.duration_since(fetch_started).as_secs_f64() * 1000.0;
    let source_id = format!("{}_result", dataset);
    let source = MetricSource {
        source_id: &source_id,
        source_type: "bi_datasets",
        status: status.as_u16(),
        latency,
        size: body_json_len,
        dataset_id: dataset,
        data: &request_body,
        info: (!query.is_empty()).then(|| json!({"query": query})),
    };
    let completed = std::time::Instant::now();
    let timings = MetricTimings::from_native_stages(started, fetch_started, fetch_ended, completed);
    let conf = json!({"type": "metric_wizard_node", "meta": {"stype": "metric_wizard_node"}, "entryId": meta_entry_id, "key": meta_key, "revId": meta_rev});
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(source_id.as_str(), source);
    let mut data_export = std::collections::BTreeMap::new();
    data_export.insert(source_id.as_str(), &data_export_value);
    let envelope = MetricEnvelope {
        sources,
        ui_scheme: None,
        params: &params,
        used_params: &used_params,
        action_params: EmptyObject {},
        widget_config: EmptyObject {},
        default_params: EmptyObject {},
        extra,
        timings,
        data: &data,
        config,
        highcharts_config: highcharts,
        data_export,
        conf_storage_config: &conf,
        key: &meta_key,
        id: response_id,
        kind: "metric_wizard_node",
        rev_id: &meta_rev,
    };
    let mut buffer = BytesMut::with_capacity(8 * 1024);
    {
        let mut writer = (&mut buffer).writer();
        serde_json::to_writer(&mut writer, &envelope)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(response_body(buffer))
        .expect("metric response"))
}

// ---------------------------------------------------------------------------
// `/api/run` raw-body entry: typed metric fast path
// ---------------------------------------------------------------------------

/// `/api/run` with the body still available as bytes. The metric chart (the benchmark
/// workload, and the lightest widget in a dashboard) is recognised by a typed parse that
/// skips the parts of the widget config the chart builder never reads, so the ~5 KB body is
/// not materialised as a full `Value` tree. Every other shape, and malformed input, falls
/// back to the legacy `Json<Value>` extraction with its exact rejection response.
pub async fn ui_run_raw(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response, (StatusCode, String)> {
    if is_json_content_type(&headers) {
        if let Some(response) = try_metric_fast(&state, &headers, &body).await {
            return response;
        }
    } else {
        std::hint::cold_path();
        // `Json` would have rejected this before parsing; keep the same 415 contract.
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`".to_string(),
        ));
    }
    match axum::Json::<Value>::from_bytes(&body) {
        Ok(Json(raw_body)) => {
            ui_run_inner(axum::extract::State(state), headers, Json(raw_body)).await
        }
        Err(rejection) => {
            std::hint::cold_path();
            Ok(rejection.into_response())
        }
    }
}

fn is_json_content_type(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let value = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            value == "application/json" || value.ends_with("+json")
        })
        .unwrap_or(false)
}

/// The `/api/run` members the metric fast path needs. Unknown members are skipped by
/// `serde` without allocating (the bulk of an inline widget config is never touched by the
/// metric builder); the fields that are needed are rebuilt verbatim into the request `Value`
/// `ui_run_inner` consumes, so behaviour is unchanged.
#[derive(serde::Deserialize)]
struct FastRun {
    id: Option<String>,
    params: Option<Value>,
    config: Option<FastConfig>,
    #[serde(rename = "revId")]
    rev_id: Option<Value>,
    #[serde(rename = "revisionId")]
    revision_id: Option<Value>,
    #[serde(rename = "isEditMode")]
    is_edit_mode: Option<Value>,
    #[serde(rename = "editMode")]
    edit_mode: Option<Value>,
    unreleased: Option<Value>,
}

#[derive(serde::Deserialize)]
struct FastConfig {
    data: Option<FastConfigData>,
    meta: Option<Value>,
    #[serde(rename = "entryId")]
    entry_id: Option<Value>,
}

#[derive(serde::Deserialize)]
struct FastConfigData {
    shared: Option<FastShared>,
}

#[derive(serde::Deserialize)]
struct FastShared {
    #[serde(rename = "datasetsIds")]
    datasets_ids: Option<Vec<String>>,
    visualization: Option<FastVisualization>,
    #[serde(rename = "extraSettings")]
    extra_settings: Option<Value>,
    #[serde(rename = "sourceType")]
    source_type: Option<Value>,
}

#[derive(serde::Deserialize)]
struct FastVisualization {
    id: Option<String>,
    placeholders: Option<Vec<FastPlaceholder>>,
}

#[derive(serde::Deserialize)]
struct FastPlaceholder {
    #[serde(rename = "type")]
    kind: Option<String>,
    items: Option<Vec<Value>>,
}

/// `Some(response)` when the body is an inline metric chart this path can rebuild exactly;
/// `None` means "use the legacy extraction" (saved charts, other chart types, odd shapes).
async fn try_metric_fast(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Option<Result<axum::response::Response, (StatusCode, String)>> {
    // Cheap pre-filter: only bodies naming the metric node take the typed parse at all.
    let text = std::str::from_utf8(body).ok()?;
    if !text.contains("metric_wizard_node") {
        return None;
    }
    let run: FastRun = serde_json::from_str(text).ok()?;
    // A metric `/api/run` always carries an id; empty id would reach the control-v2 branch
    // that reads much more of the config, so leave it to the legacy path.
    let id = run.id.filter(|id| !id.is_empty())?;
    let config = run.config?;
    let meta = config.meta?;
    if meta.get("stype").and_then(Value::as_str) != Some("metric_wizard_node") {
        return None;
    }
    let shared = config.data?.shared?;
    let visualization = shared.visualization?;
    if visualization.id.as_deref() != Some("metric") {
        return None;
    }
    let placeholders = visualization.placeholders?;
    let measure_items = placeholders
        .into_iter()
        .find(|placeholder| placeholder.kind.as_deref() == Some("measures"))?
        .items?;
    if measure_items.is_empty() {
        return None;
    }

    // Rebuild only the members the dispatch reads. `run_metric_rust` uses `datasetsIds`,
    // `visualization.placeholders[measures].items`, `extraSettings` and the measure item;
    // `ui_run_inner` also reads `data.shared.sourceType`, `config.meta`/`entryId` and the
    // request head fields below.
    let mut shared_map = serde_json::Map::with_capacity(4);
    if let Some(datasets_ids) = shared.datasets_ids {
        shared_map.insert(
            "datasetsIds".to_string(),
            Value::Array(datasets_ids.into_iter().map(Value::String).collect()),
        );
    }
    let mut placeholder_map = serde_json::Map::with_capacity(2);
    placeholder_map.insert("type".to_string(), Value::String("measures".to_string()));
    placeholder_map.insert("items".to_string(), Value::Array(measure_items));
    let mut visualization_map = serde_json::Map::with_capacity(2);
    visualization_map.insert("id".to_string(), Value::String("metric".to_string()));
    visualization_map.insert(
        "placeholders".to_string(),
        Value::Array(vec![Value::Object(placeholder_map)]),
    );
    shared_map.insert("visualization".to_string(), Value::Object(visualization_map));
    if let Some(extra_settings) = shared.extra_settings {
        shared_map.insert("extraSettings".to_string(), extra_settings);
    }
    if let Some(source_type) = shared.source_type {
        shared_map.insert("sourceType".to_string(), source_type);
    }
    let mut data_map = serde_json::Map::with_capacity(1);
    data_map.insert("shared".to_string(), Value::Object(shared_map));
    let mut config_map = serde_json::Map::with_capacity(3);
    config_map.insert("data".to_string(), Value::Object(data_map));
    config_map.insert("meta".to_string(), meta);
    if let Some(entry_id) = config.entry_id {
        config_map.insert("entryId".to_string(), entry_id);
    }
    let mut body_map = serde_json::Map::with_capacity(9);
    body_map.insert("id".to_string(), Value::String(id));
    if let Some(params) = run.params {
        body_map.insert("params".to_string(), params);
    }
    body_map.insert("config".to_string(), Value::Object(config_map));
    if let Some(value) = run.rev_id {
        body_map.insert("revId".to_string(), value);
    }
    if let Some(value) = run.revision_id {
        body_map.insert("revisionId".to_string(), value);
    }
    if let Some(value) = run.is_edit_mode {
        body_map.insert("isEditMode".to_string(), value);
    }
    if let Some(value) = run.edit_mode {
        body_map.insert("editMode".to_string(), value);
    }
    if let Some(value) = run.unreleased {
        body_map.insert("unreleased".to_string(), value);
    }
    Some(
        ui_run_inner(
            axum::extract::State(Arc::clone(state)),
            headers.clone(),
            Json(Value::Object(body_map)),
        )
        .await,
    )
}

fn interval_values(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("__interval_")?;
    let (from, to) = rest.split_once('_')?;
    Some((from.to_string(), to.to_string()))
}

fn numeric_compare(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let mut left_chars = left.chars().peekable();
    let mut right_chars = right.chars().peekable();
    loop {
        match (left_chars.peek().copied(), right_chars.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                let mut a_number = String::new();
                while let Some(ch) = left_chars.peek().copied() {
                    if !ch.is_ascii_digit() {
                        break;
                    }
                    a_number.push(ch);
                    left_chars.next();
                }
                let mut b_number = String::new();
                while let Some(ch) = right_chars.peek().copied() {
                    if !ch.is_ascii_digit() {
                        break;
                    }
                    b_number.push(ch);
                    right_chars.next();
                }
                let a_trimmed = a_number.trim_start_matches('0');
                let b_trimmed = b_number.trim_start_matches('0');
                let ordering = a_trimmed
                    .len()
                    .cmp(&b_trimmed.len())
                    .then_with(|| a_trimmed.cmp(b_trimmed));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(a), Some(b)) => {
                let ordering = a.cmp(&b);
                if ordering != Ordering::Equal {
                    return ordering;
                }
                left_chars.next();
                right_chars.next();
            }
        }
    }
}

fn build_distincts_body(shared: &Value, params: &Value, fields: &Value) -> Value {
    let source = shared.get("source").cloned().unwrap_or_else(|| json!({}));
    let param = source
        .get("datasetFieldId")
        .and_then(|value| value.as_str())
        .or_else(|| source.get("fieldName").and_then(|value| value.as_str()))
        .unwrap_or_default();
    let mut by_name: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    if let Some(fields) = fields.as_array() {
        for field in fields {
            let Some(guid) = field.get("guid").and_then(|value| value.as_str()) else {
                continue;
            };
            let info = json!({
                "guid": guid,
                "fieldType": field.get("type").cloned().unwrap_or(Value::Null),
                "calc_mode": field.get("calc_mode").cloned().unwrap_or(Value::Null),
            });
            by_name.insert(guid.to_string(), info.clone());
            if let Some(title) = field.get("title").and_then(|value| value.as_str()) {
                by_name.insert(title.to_string(), info);
            }
        }
    }
    let target_guid = by_name
        .get(param)
        .and_then(|info| info.get("guid"))
        .and_then(|value| value.as_str())
        .unwrap_or(param)
        .to_string();
    let mut filters = Vec::new();
    let mut parameter_values = Vec::new();
    if let Some(params) = params.as_object() {
        for (key, value) in params {
            let info = by_name.get(key);
            let guid = info
                .and_then(|info| info.get("guid"))
                .and_then(|value| value.as_str())
                .unwrap_or(key);
            let field_type = info
                .and_then(|info| info.get("fieldType"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if field_type == "MEASURE" {
                continue;
            }
            if let Some(calc_mode) = info
                .and_then(|info| info.get("calc_mode"))
                .and_then(|value| value.as_str())
            {
                if calc_mode == "parameter" {
                    let first = value
                        .as_array()
                        .and_then(|items| items.first())
                        .cloned()
                        .unwrap_or_else(|| value.clone());
                    if !first.is_null() && first.as_str() != Some("") {
                        parameter_values
                            .push(json!({"ref": {"type": "id", "id": guid}, "value": first}));
                    }
                }
            }
            if key == param {
                continue;
            }
            let mut values = Vec::new();
            let mut operation = "IN".to_string();
            if let Some(items) = value.as_array() {
                for item in items {
                    if item.is_null() || item.as_str() == Some("") {
                        continue;
                    }
                    if let Some(text) = item.as_str() {
                        if let Some((from, to)) = interval_values(text) {
                            values = vec![json!(from), json!(to)];
                            operation = "BETWEEN".to_string();
                            continue;
                        }
                    }
                    values.push(item.clone());
                }
            }
            let id = by_name
                .get(key)
                .and_then(|info| info.get("guid"))
                .and_then(|value| value.as_str())
                .unwrap_or(key);
            if id == target_guid || values.is_empty() {
                continue;
            }
            filters.push(json!({
                "ref": {"type": "id", "id": id},
                "operation": operation,
                "values": values,
            }));
        }
    }
    json!({
        "ignore_nonexistent_filters": true,
        "fields": [{"ref": {"type": "id", "id": target_guid}, "role_spec": {"role": "distinct"}}],
        "filters": filters,
        "parameter_values": parameter_values,
    })
}

async fn control_v2(
    state: &Arc<AppState>,
    raw_body: &Value,
    params: &Value,
    started: std::time::Instant,
    scope: &RequestScope,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // No-JS build: params are resolved by the Rust port. `__relative` dates are not
    // supported, the same guard the pivot/table fast paths use.
    let params = crate::pivot_engine::resolve_params(params)
        .map_err(|error| (StatusCode::BAD_REQUEST, format!("control params: {error}")))?;
    let shared = raw_body
        .pointer("/config/data/shared")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let source = shared.get("source").cloned().unwrap_or_else(|| json!({}));
    let source_type = shared
        .get("sourceType")
        .and_then(|value| value.as_str())
        .unwrap_or("dataset");
    let element = source
        .get("elementType")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let param = source
        .get("datasetFieldId")
        .and_then(|value| value.as_str())
        .or_else(|| source.get("fieldName").and_then(|value| value.as_str()))
        .unwrap_or_default()
        .to_string();
    let mut final_params = params.clone();
    let mut used_params = json!({});
    let mut sources = json!({});
    let mut data_export = json!({});
    let mut extra = json!({});
    let mut content = source
        .get("acceptableValues")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut data_latency = 0u64;
    if source_type == "dataset" {
        let dataset_id = source
            .get("datasetId")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let fields_started = std::time::Instant::now();
        let (fields, revision) = dataset_fields(state, scope, &dataset_id).await?;
        // Exact `{"fields":…,"revision_id":…}` length without building or serializing it.
        let fields_body_len = fields_body_json_len(&fields, &revision);
        let status = StatusCode::OK;
        let fields_latency = fields_started.elapsed().as_millis() as u64;
        data_latency += fields_latency;
        let mut fields_map = serde_json::Map::new();
        let mut fields_list = Vec::new();
        let mut field_params = serde_json::Map::new();
        if let Some(items) = fields.as_array() {
            for field in items {
                let Some(guid) = field.get("guid").and_then(|value| value.as_str()) else {
                    continue;
                };
                let title = field.get("title").cloned().unwrap_or(Value::Null);
                fields_map.insert(guid.to_string(), title.clone());
                field_params.insert(guid.to_string(), json!(""));
                fields_list.push(json!({
                    "title": title,
                    "guid": guid,
                    "dataType": field.get("data_type").cloned().unwrap_or(Value::Null),
                    "calc_mode": field.get("calc_mode").cloned().unwrap_or(Value::Null),
                    "fieldType": field.get("type").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        update_params(
            &mut final_params,
            &mut used_params,
            &Value::Object(field_params),
        );
        extra = json!({
            "datasets": [{
                "fields": fields_map,
                "fieldsList": fields_list,
                "id": dataset_id,
            }]
        });
        if let Some(map) = sources.as_object_mut() {
            map.insert(
                "fields".to_string(),
                json!({
                    "sourceId": "fields",
                    "sourceType": "bi_datasets",
                    "status": status.as_u16(),
                    "latency": fields_latency,
                    "size": fields_body_len,
                    "datasetId": dataset_id,
                }),
            );
        }
        let field_type = source
            .get("datasetFieldType")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if element != "date"
            && element != "input"
            && element != "checkbox"
            && field_type != "MEASURE"
        {
            let distincts_body = build_distincts_body(&shared, &params, &fields);
            // The revision pins the data-api query-cache key. It travels out of band (not in
            // `distincts_body`) so `sources.distincts.data` stays byte-identical to the
            // reference request body.
            let revision_str = revision.as_str();
            let distincts_started = std::time::Instant::now();
            enum DistinctBody {
                Typed(crate::typed_data::DataBody),
                Json(Value),
            }
            let (status, body_source, body_json_len, export) = if crate::data_direct::installed()
            {
                match crate::data_direct::distinct_typed(
                    &state.http,
                    &state.data_api,
                    &scope.headers,
                    &dataset_id,
                    &distincts_body,
                    revision_str,
                )
                .await
                .map_err(|(status, message)| (status, format!("control distincts: {message}")))?
                {
                    crate::data_direct::TypedResult::Ok(outcome) => (
                        outcome.status,
                        DistinctBody::Typed(outcome.body),
                        outcome.json_len,
                        Some(outcome.data_export),
                    ),
                    // Same as the HTTP path: the control still answers (empty content), with
                    // the data-api status in `sources.distincts`.
                    crate::data_direct::TypedResult::Failed { status, body } => {
                        (status, DistinctBody::Json(body), 0, None)
                    }
                }
            } else {
                let outcome = crate::data_direct::distinct(
                    &state.http,
                    &state.data_api,
                    &scope.headers,
                    &dataset_id,
                    &distincts_body,
                )
                .await
                .map_err(|(status, message)| (status, format!("control distincts: {message}")))?;
                (
                    outcome.status,
                    DistinctBody::Json(outcome.body),
                    outcome.json_len,
                    outcome.data_export,
                )
            };
            let latency = distincts_started.elapsed().as_millis() as u64;
            data_latency += latency;
            let size = if body_json_len > 0 {
                body_json_len
            } else {
                match &body_source {
                    DistinctBody::Json(body) => dl_api::json_len(body),
                    DistinctBody::Typed(_) => 0,
                }
            };
            let mut distincts = serde_json::Map::new();
            distincts.insert("sourceId".to_string(), json!("distincts"));
            distincts.insert("sourceType".to_string(), json!("bi_datasets"));
            distincts.insert("status".to_string(), json!(status.as_u16()));
            distincts.insert("latency".to_string(), json!(latency));
            distincts.insert("size".to_string(), json!(size));
            distincts.insert("datasetId".to_string(), json!(dataset_id));
            distincts.insert("data".to_string(), distincts_body);
            if let Some(map) = sources.as_object_mut() {
                map.insert("distincts".to_string(), Value::Object(distincts));
            }
            if let Some(export) = export {
                if let Some(map) = data_export.as_object_mut() {
                    map.insert("distincts".to_string(), export);
                }
            }
            let mut values: Vec<(String, Value)> = match &body_source {
                DistinctBody::Typed(body) => body
                    .as_result()
                    .map(|result| {
                        result
                            .rows()
                            .iter()
                            .filter_map(|row| {
                                let value = row.data.first().cloned().unwrap_or(Value::Null);
                                let title = value
                                    .as_str()
                                    .map(|text| text.to_string())
                                    .unwrap_or_else(|| value.to_string());
                                Some((title, value))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                DistinctBody::Json(body) => body
                    .pointer("/result_data/0/rows")
                    .and_then(|value| value.as_array())
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| {
                                let value =
                                    row.pointer("/data/0").cloned().unwrap_or(Value::Null);
                                let title = value
                                    .as_str()
                                    .map(|text| text.to_string())
                                    .unwrap_or_else(|| value.to_string());
                                Some((title, value))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            if field_type == "float" {
                values.sort_by(|left, right| {
                    let a = left.0.parse::<f64>().unwrap_or(f64::NAN);
                    let b = right.0.parse::<f64>().unwrap_or(f64::NAN);
                    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
                });
            } else {
                values.sort_by(|left, right| numeric_compare(&left.0, &right.0));
            }
            content = values
                .into_iter()
                .map(|(title, value)| json!({"title": title, "value": value}))
                .collect();
        }
    }
    let mut ui_control = serde_json::Map::new();
    let show_title = source
        .get("showTitle")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    ui_control.insert(
        "label".to_string(),
        if show_title {
            shared.get("title").cloned().unwrap_or_else(|| json!(""))
        } else {
            json!("")
        },
    );
    if let Some(multiselect) = source.get("multiselectable") {
        ui_control.insert("multiselect".to_string(), multiselect.clone());
    }
    ui_control.insert("content".to_string(), Value::Array(content));
    ui_control.insert("param".to_string(), json!(param));
    if let Some(required) = source.get("required") {
        ui_control.insert("required".to_string(), required.clone());
    }
    if let Some(disabled) = shared.get("disabled") {
        ui_control.insert("disabled".to_string(), disabled.clone());
    }
    let element_type = if source_type == "manual" {
        source
            .get("elementType")
            .and_then(|value| value.as_str())
            .unwrap_or("select")
    } else {
        element
    };
    let mut control_type = element_type.to_string();
    let is_date = element_type == "date";
    if is_date && source.get("isRange").and_then(|value| value.as_bool()) == Some(true) {
        control_type = "range-datepicker".to_string();
        if let Some(default_value) = source.get("defaultValue").and_then(|value| value.as_str()) {
            if let Some((from, to)) = interval_values(default_value) {
                // Prefer the already resolved param (processor resolveParams
                // turns __relative parts into absolute timestamps).
                let resolved = final_params
                    .get(&param)
                    .and_then(|value| value.as_array())
                    .and_then(|items| items.first())
                    .and_then(|value| value.as_str())
                    .filter(|text| text.starts_with("__interval_"))
                    .map(|text| text.to_string());
                let value = resolved.unwrap_or_else(|| format!("__interval_{}_{}", from, to));
                if let Some(params) = final_params.as_object_mut() {
                    params.insert(param.clone(), json!(value));
                }
            }
        } else if !ui_control
            .get("content")
            .and_then(|value| value.as_array())
            .map(|items| items.is_empty())
            .unwrap_or(true)
        {
            if let Some(items) = ui_control.get("content").and_then(|value| value.as_array()) {
                let from = items
                    .first()
                    .and_then(|item| item.get("value"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("0001-01-01");
                let to = items
                    .last()
                    .and_then(|item| item.get("value"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("9999-12-31");
                if let Some(params) = final_params.as_object_mut() {
                    params.insert(param.clone(), json!(format!("__interval_{}_{}", from, to)));
                }
            }
        }
    } else if is_date {
        control_type = "datepicker".to_string();
        if let Some(default_value) = source.get("defaultValue").and_then(|value| value.as_str()) {
            if let Some(params) = final_params.as_object_mut() {
                params.insert(param.clone(), json!(default_value));
            }
        }
    } else {
        let missing = final_params
            .get(&param)
            .map(|value| match value {
                Value::Array(items) => items.is_empty(),
                Value::String(text) => text.is_empty(),
                Value::Null => true,
                _ => false,
            })
            .unwrap_or(true);
        if missing {
            let default = source.get("defaultValue").cloned().unwrap_or(Value::Null);
            let value = if element_type == "select" {
                match default {
                    Value::Array(items) if !items.is_empty() => Value::Array(items),
                    Value::Array(_) | Value::Null => json!([""]),
                    other => json!([other]),
                }
            } else if element_type == "checkbox" {
                if default.is_null() {
                    json!(["false"])
                } else {
                    json!([default])
                }
            } else if default.is_null() {
                json!([""])
            } else {
                json!([default])
            };
            if let Some(params) = final_params.as_object_mut() {
                params.insert(param.clone(), value);
            }
        }
    }
    ui_control.insert("type".to_string(), json!(control_type));
    let mut control_override = serde_json::Map::new();
    if let Some(value) = final_params.get(&param) {
        control_override.insert(param.clone(), value.clone());
    }
    update_params(
        &mut final_params,
        &mut used_params,
        &Value::Object(control_override),
    );
    let colors = system_palettes()
        .get(state.default_color_palette.as_str())
        .and_then(|palette| palette.get("scheme"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    if let Some(object) = extra.as_object_mut() {
        if let Some(colors) = colors.as_array() {
            if !colors.is_empty() {
                object.insert("colors".to_string(), Value::Array(colors.clone()));
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    let config_ms = (elapsed - data_latency as f64).max(0.0);
    Ok(axum::response::Json(json!({
        "sources": sources,
        "uiScheme": [Value::Object(ui_control)],
        "params": final_params,
        "usedParams": used_params,
        "actionParams": {},
        "widgetConfig": {},
        "defaultParams": {},
        "extra": extra,
        "timings": {"configResolving": config_ms, "dataFetching": data_latency as f64, "jsExecution": 0.0},
        "data": {},
        "config": "{\"notOverlayControls\":true}",
        "highchartsConfig": "{}",
        "dataExport": data_export,
        "_confStorageConfig": {"type": "control_dash", "meta": {"stype": "control_dash"}},
        "type": "control_dash",
    })).into_response())
}

async fn ui_run_inner(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(raw_body): Json<Value>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    validate_tenant(&state, &headers)?;
    let started = std::time::Instant::now();
    let lang = headers
        .get("accept-language")
        .and_then(|value| value.to_str().ok())
        .map(|value| if value.starts_with("ru") { "ru" } else { "en" })
        .unwrap_or("en");
    let params = normalized_params(raw_body.get("params"));
    let scope = build_scope(&state, &headers, &raw_body);
    let request_id = raw_body
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    // The inline `shared` config is several kilobytes of nested JSON and the chart
    // builders only read it, so borrow it from the request body instead of deep-cloning
    // it on every `/api/run`.
    let body_shared = raw_body.pointer("/config/data/shared");
    let body_source_type = body_shared
        .and_then(|shared| shared.get("sourceType"))
        .and_then(|value| value.as_str());
    if request_id.is_empty() && body_source_type.is_some() {
        // Controls read a dataset too. One request-scoped bundle makes the data-api resolver
        // hand `/fields` and `values/distinct` the same `Arc<Dataset>` instead of reading US
        // on every call (the typed path below does not go through the HTTP router).
        let dataset_ids = control_dataset_ids(&raw_body);
        let bundle = crate::us_direct::bundle(None, &dataset_ids, &scope.branch).await;
        return crate::us_direct::with_bundle(
            bundle,
            control_v2(&state, &raw_body, &params, started, &scope),
        )
        .await;
    }
    let mut chart_id = request_id.clone();
    if chart_id.is_empty() {
        if let Some(id) = raw_body
            .pointer("/config/meta/entryId")
            .or_else(|| raw_body.pointer("/config/entryId"))
            .and_then(|value| value.as_str())
        {
            chart_id = id.to_string();
        }
    }
    if chart_id.is_empty()
        && body_shared
            .and_then(|shared| shared.pointer("/visualization"))
            .is_none()
    {
        if let Some(fallback) = &state.node_fallback {
            let response = state
                .http
                .post(fallback)
                .json(&raw_body)
                .send()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("fallback: {e}")))?;
            let status = response.status();
            let body: Value = response
                .json()
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("fallback json: {e}")))?;
            if !status.is_success() {
                return Err((StatusCode::BAD_GATEWAY, format!("fallback: {status}")));
            }
            return Ok(axum::response::Json(body).into_response());
        }
        return Err((StatusCode::BAD_REQUEST, "chart id is required".to_string()));
    }
    // v5.2: an inline `shared` config needs no chart entry read; a saved chart does.
    let inline_shared = body_shared.filter(|shared| shared.pointer("/visualization").is_some());
    let has_inline_shared = inline_shared.is_some();
    // v5.2: one request-scoped US bundle — chart entry (when it is not inline), its
    // datasets and the datasets' connections — read once from the in-process US model.
    //
    // The bundle is built BEFORE the chart entry is examined. For a saved chart it reads the
    // entry itself and extracts the datasets from its `shared` config, so the dispatch below
    // finds the entry in the bundle instead of issuing a second US read: the old order read
    // the same 32-104 KB entry twice per request. `with_bundle` makes the bundle visible to
    // every US read inside the dispatch (engine chart entry and data-api typed dataset
    // resolution), so the chart path never uses the legacy `local_us` channel.
    // The inline branch's `meta` is read here, outside the dispatch future: `raw_body` is
    // borrowed by `body_shared`, so it must not be moved into the future as well.
    let inline_meta = if has_inline_shared {
        raw_body
            .pointer("/config/meta")
            .cloned()
            .unwrap_or_else(|| json!({}))
    } else {
        Value::Null
    };
    let bundle_entry_id = if has_inline_shared {
        None
    } else {
        Some(chart_id.clone())
    };
    let bundle_dataset_ids = match inline_shared {
        Some(shared) => crate::us_direct::dataset_ids(shared),
        None => Vec::new(),
    };
    let bundle = crate::us_direct::bundle(
        bundle_entry_id.as_deref(),
        &bundle_dataset_ids,
        &scope.branch,
    )
    .await;
    return crate::us_direct::with_bundle(bundle, async move {
    // The chart entry and `shared` config are read *inside* the bundle scope: a saved chart
    // hits the bundle (no SQL, no second JSON parse); an inline config needs no read at all.
    //
    // `entry` is read once and kept as `Arc` for the whole dispatch; `shared` is either parsed
    // from the stored JSON string (the common US shape) or **borrowed** from the entry, so the
    // 32-104 KB config is not deep-cloned on every chart.
    let entry: Option<std::sync::Arc<Value>> = if inline_shared.is_some() {
        None
    } else {
        Some(crate::us_direct::entry_json(&chart_id, &scope.branch, None).await?)
    };
    let shared_owned: Option<std::sync::Arc<Value>> = match entry
        .as_ref()
        .and_then(|entry| entry.pointer("/data/shared"))
    {
        Some(Value::String(text)) => {
            let revision = entry
                .as_ref()
                .and_then(|entry| entry.get("revId"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(parsed_shared_cached(
                &chart_id,
                &scope.branch,
                revision,
                text,
            ).ok_or_else(|| (StatusCode::BAD_GATEWAY, "shared: invalid JSON".to_string()))?)
        }
        _ => None,
    };
    let (config_type, entry_id, key, rev, response_id) = match entry.as_ref() {
        Some(entry) => {
            let entry_type = entry
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("graph_wizard_node");
            let config_type = config_type_from_entry(entry_type);
            let entry_id = entry
                .get("entryId")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| json!(chart_id.clone()));
            let key = entry.get("key").cloned().unwrap_or(Value::Null);
            let rev = entry.get("revId").cloned().unwrap_or(Value::Null);
            let response_id = entry_id
                .as_str()
                .map(|value| value.to_string())
                .unwrap_or_else(|| chart_id.clone());
            (config_type, entry_id, key, rev, response_id)
        }
        None => {
            let meta = inline_meta;
            let config_type = meta
                .get("stype")
                .and_then(|value| value.as_str())
                .unwrap_or("d3_wizard_node")
                .to_string();
            let entry_id = meta
                .get("entryId")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or_else(|| json!(request_id.clone()));
            let key = meta.get("key").cloned().unwrap_or(Value::Null);
            let rev = meta.get("revId").cloned().unwrap_or(Value::Null);
            let response_id = entry_id
                .as_str()
                .map(|value| value.to_string())
                .unwrap_or_else(|| request_id.clone());
            (config_type, entry_id, key, rev, response_id)
        }
    };
    let shared: &Value = match (
        &shared_owned,
        inline_shared,
        entry
            .as_ref()
            .and_then(|entry| entry.pointer("/data/shared")),
    ) {
        (Some(shared), _, _) => shared.as_ref(),
        (None, Some(shared), _) => shared,
        (None, None, Some(shared)) => shared,
        _ => {
            return Err((
                StatusCode::BAD_GATEWAY,
                "chart entry has no shared config".to_string(),
            ))
        }
    };
    // The first real builder failure (data-api/config/oversize) is reported to the
    // client; a `501 is not ported` stays a 501 only when nothing better failed.
    let mut failure: Option<(StatusCode, String)> = None;
    if (config_type == "graph_wizard_node" || config_type == "d3_wizard_node") && !rust_disabled() {
        let graph_entry_id = entry_id.as_str().unwrap_or_default().to_string();
        match run_rust_graph(
            &state,
            shared,
            &params,
            &graph_entry_id,
            entry_id.clone(),
            key.clone(),
            rev.clone(),
            &response_id,
            started,
            &scope,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err((status, message)) => {
                std::hint::cold_path();
                if status != StatusCode::NOT_IMPLEMENTED && failure.is_none() {
                    failure = Some((status, message.clone()));
                }
                tracing::warn!(%message, "rust graph fallback")
            }
        }
    }
    if config_type == "ymap_wizard_node" && !rust_disabled() {
        let geo_entry_id = entry_id.as_str().unwrap_or_default().to_string();
        match run_rust_geo(
            &state,
            shared,
            &params,
            entry_id.clone(),
            key.clone(),
            rev.clone(),
            &geo_entry_id,
            started,
            &scope,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err((status, message)) => {
                std::hint::cold_path();
                if status != StatusCode::NOT_IMPLEMENTED && failure.is_none() {
                    failure = Some((status, message.clone()));
                }
                tracing::warn!(%message, "rust geo fallback");
            }
        }
    }
    if config_type == "table_wizard_node"
        && !rust_disabled()
        && shared.pointer("/visualization/id").and_then(Value::as_str) == Some("pivotTable")
    {
        let pivot_entry_id = entry_id.as_str().unwrap_or_default().to_string();
        match run_rust_pivot(
            &state,
            shared,
            &params,
            lang,
            &pivot_entry_id,
            entry_id.clone(),
            key.clone(),
            rev.clone(),
            &response_id,
            started,
            &scope,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err((status, message)) => {
                std::hint::cold_path();
                if status != StatusCode::NOT_IMPLEMENTED && failure.is_none() {
                    failure = Some((status, message.clone()));
                }
                tracing::warn!(%message, "rust pivot fallback")
            }
        }
    }
    if config_type == "table_wizard_node" && !rust_disabled() {
        let table_entry_id = entry_id.as_str().unwrap_or_default().to_string();
        match run_rust_table(
            &state,
            shared,
            &params,
            lang,
            &table_entry_id,
            entry_id.clone(),
            key.clone(),
            rev.clone(),
            &response_id,
            started,
            &scope,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err((status, message)) => {
                std::hint::cold_path();
                if status != StatusCode::NOT_IMPLEMENTED && failure.is_none() {
                    failure = Some((status, message.clone()));
                }
                tracing::warn!(%message, "rust table fallback")
            }
        }
    }
    if config_type == "metric_wizard_node" && !rust_disabled() {
        let metric_entry_id = entry_id.as_str().unwrap_or_default().to_string();
        match run_metric_rust(
            &state,
            shared,
            &params,
            lang,
            &metric_entry_id,
            entry_id.clone(),
            key.clone(),
            rev.clone(),
            &response_id,
            started,
            &scope,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err((status, message)) => {
                if status != StatusCode::NOT_IMPLEMENTED && failure.is_none() {
                    failure = Some((status, message));
                }
            }
        }
        std::hint::cold_path();
    }
    if let Some((status, message)) = failure {
        return Ok(error_response(status, &message));
    }
    // Every chart type the UI serves has a Rust builder. Anything else (geo, QL,
    // arbitrary widgets) is reported instead of being sent to a JS engine.
    std::hint::cold_path();
    tracing::warn!(
        config_type = %config_type,
        visualization = %shared.pointer("/visualization/id").and_then(|value| value.as_str()).unwrap_or(""),
        "unsupported Rust chart type"
    );
    Err((
        StatusCode::NOT_IMPLEMENTED,
        format!("chart type {config_type} is not supported by the Rust engine"),
    ))
    })
    .await;
}

/// The buffered handler, kept for callers that do not stream.
pub async fn ui_run(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(raw_body): Json<Value>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    ui_run_inner(
        axum::extract::State(state),
        headers,
        Json(raw_body),
    )
    .await
}

pub async fn cache_stats(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<Value> {
    let size = state
        .fields_cache
        .lock()
        .map(|cache| cache.len())
        .unwrap_or(0);
    let flights = state.fields_flight.lock().await.len();
    Json(json!({
        "size": size,
        "flights": flights,
        "hits": state.fields_hits.load(std::sync::atomic::Ordering::Relaxed),
        "misses": state.fields_misses.load(std::sync::atomic::Ordering::Relaxed),
        "fetches": state.fields_fetches.load(std::sync::atomic::Ordering::Relaxed),
        "invalidations": state.fields_invalidations.load(std::sync::atomic::Ordering::Relaxed),
        "ttl_secs": state.fields_ttl.as_secs(),
        "max": state.fields_max,
        "auth_enabled": state.auth_enabled,
        "cache_auth": state.fields_cache_auth,
        // v9.0: per-connection query-result cache from the typed data-api service.
        "query_cache": crate::data_direct::query_cache_json(),
        "decode_request": crate::data_direct::decode_stats_json(),
        // v9.10: revision-validated US metadata cache (`stale` counts hits rejected by the
        // saved/published/updated_at token, i.e. an edit was seen immediately).
        "entry_cache": crate::us_direct::entry_cache_json(),
    }))
}

/// v5.2 batch entry point: one HTTP request for several chart bodies. The UI currently uses
/// individual `/api/run` calls, but this lets a client or test fan out widgets without paying
/// 21 HTTP round-trips. Work runs concurrently and preserves each response envelope/status.
pub async fn batch_run(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response, (StatusCode, String)> {
    if !is_json_content_type(&headers) {
        std::hint::cold_path();
        // `Json` would have rejected this before parsing; keep the same 415 contract.
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`".to_string(),
        ));
    }
    // Keep the request body as `Bytes`: split the request array into zero-copy spans here,
    // then let each spawned chart task parse its own body (the 44 KB batch JSON parse moves
    // off the request task and runs in parallel with the chart builds). A malformed body
    // still gets the exact `Json` rejection response.
    if serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_err() {
        std::hint::cold_path();
        return match axum::Json::<Value>::from_bytes(&body) {
            Ok(_) => Err((
                StatusCode::BAD_REQUEST,
                "batch must be an array or {requests:[...]}".to_string(),
            )),
            Err(rejection) => Ok(rejection.into_response()),
        };
    }
    let items: Vec<bytes::Bytes> = match split_batch_requests(&body) {
        Some(items) => items,
        None => {
            std::hint::cold_path();
            return Err((
                StatusCode::BAD_REQUEST,
                "batch must be an array or {requests:[...]}".to_string(),
            ));
        }
    };
    if items.len() > 64 {
        std::hint::cold_path();
        return Err((StatusCode::BAD_REQUEST, "batch limit is 64".to_string()));
    }
    // Run every chart body as its own tokio task. The old shape built `Vec<async block>`
    // and `join_all`ed it: that polls all futures on the *single* request task, and the
    // per-chart work is CPU-bound with in-process (non-yielding) awaits, so the charts
    // effectively executed one after another on one worker. Spawning lets the runtime put
    // different charts on different workers. Results are re-collected in spawn order, so
    // the batch envelope order is unchanged.
    //
    // `DL_BATCH_CONCURRENCY` caps how many bodies of one batch request may execute at once
    // (default 8: a 5-chart dashboard is unaffected, a 64-entry batch cannot have all of
    // its chart buffers live simultaneously). 0 = uncapped (work-conserving).
    let cap = batch_concurrency();
    let semaphore = (cap > 0).then(|| Arc::new(tokio::sync::Semaphore::new(cap)));
    let count = items.len();
    // `JoinSet`, not `Vec<JoinHandle>` + `join_all`: a dropped `JoinHandle` only detaches, so a
    // client that closed the tab left every unfinished chart querying for an answer nobody
    // would read. Dropping the set aborts the remaining tasks; each task reports its own index
    // because the set yields completions out of order, and the batch envelope has to keep the
    // request order.
    let mut tasks = tokio::task::JoinSet::new();
    for (index, body) in items.into_iter().enumerate() {
        let state = Arc::clone(&state);
        let headers = headers.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = match semaphore.as_ref() {
                Some(semaphore) => Some(
                    semaphore
                        .acquire()
                        .await
                        .expect("batch concurrency semaphore is never closed"),
                ),
                None => None,
            };
            (index, batch_chart_bytes(state, headers, body).await)
        });
    }
    // A joined chart task yields `JoinError` only for cancellation/shutdown (the release
    // profile is `panic = "abort"`, so an actual panic aborts the process like before);
    // turn it into that chart's regular error entry so one failed task cannot drop its
    // siblings' responses.
    let mut collected: Vec<Option<Result<(u16, bytes::Bytes), (StatusCode, String)>>> =
        (0..count).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, result)) => collected[index] = Some(result),
            Err(error) => tracing::warn!(%error, "batch chart task failed"),
        }
    }
    let results: Vec<Result<(u16, bytes::Bytes), (StatusCode, String)>> = collected
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.unwrap_or_else(|| {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("batch task {index} failed"),
                ))
            })
        })
        .collect();
    // One pass: every chart body was already serialized to JSON by its handler, so the
    // batch envelope embeds the bytes verbatim instead of parsing each answer back into a
    // `Value` and serializing it a second (and third) time.
    let capacity = 32 + results
        .iter()
        .map(|result| match result {
            Ok((_, body)) => body.len() + 32,
            Err((_, message)) => message.len() + 48,
        })
        .sum::<usize>();
    let mut buffer = bytes::BytesMut::with_capacity(capacity);
    {
        use std::io::Write as _;
        let mut writer = (&mut buffer).writer();
        writer
            .write_all(b"{\"responses\":[")
            .map_err(batch_internal_error)?;
        for (index, result) in results.into_iter().enumerate() {
            if index > 0 {
                writer.write_all(b",").map_err(batch_internal_error)?;
            }
            match result {
                Ok((status, body)) => {
                    write!(writer, "{{\"status\":{status},\"body\":")
                        .map_err(batch_internal_error)?;
                    // The handlers serialize with `serde_json`, so the body is valid JSON;
                    // validate without building a tree and keep the old `null` fallback for
                    // anything unexpected.
                    if serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_ok() {
                        writer.write_all(&body).map_err(batch_internal_error)?;
                    } else {
                        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        serde_json::to_writer(&mut writer, &value).map_err(batch_internal_error)?;
                    }
                    writer.write_all(b"}").map_err(batch_internal_error)?;
                }
                Err((status, message)) => {
                    write!(writer, "{{\"status\":{},\"error\":", status.as_u16())
                        .map_err(batch_internal_error)?;
                    serde_json::to_writer(&mut writer, &message).map_err(batch_internal_error)?;
                    writer.write_all(b"}").map_err(batch_internal_error)?;
                }
            }
        }
        writer.write_all(b"]}").map_err(batch_internal_error)?;
    }
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(response_body(buffer))
        .expect("batch response"))
}

/// Optional per-chart deadline for a stream. The task releases its semaphore permit
/// and emits a regular per-chart error instead of occupying the batch forever.
/// `0` disables the deadline; the default is deliberately generous for large queries.
fn stream_item_timeout() -> Option<std::time::Duration> {
    static TIMEOUT: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        let seconds = std::env::var("DL_STREAM_ITEM_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(60);
        (seconds > 0).then(|| std::time::Duration::from_secs(seconds))
    })
}

/// Streaming batch entry point: same request shape as `/api/run/batch`, but the
/// response is NDJSON and each line is emitted as soon as that chart is ready.
///
/// `{"index":N,"status":200,"body":{...}}\n` on success and
/// `{"index":N,"status":400,"error":"..."}\n` on a per-chart error. The
/// compression middleware wraps this body stream, so zstd/gzip is applied per
/// emitted line rather than buffering the entire batch.
pub async fn batch_run_stream(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, (StatusCode, String)> {
    if !is_json_content_type(&headers) {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected request with `Content-Type: application/json`".to_string(),
        ));
    }
    if serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_err() {
        return match axum::Json::<Value>::from_bytes(&body) {
            Ok(_) => Err((
                StatusCode::BAD_REQUEST,
                "batch must be an array or {requests:[...]}".to_string(),
            )),
            Err(rejection) => Ok(rejection.into_response()),
        };
    }
    let items = match split_batch_requests(&body) {
        Some(items) => items,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                "batch must be an array or {requests:[...]}".to_string(),
            ));
        }
    };
    if items.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "batch limit is 64".to_string()));
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    let cap = batch_concurrency();
    let semaphore = (cap > 0).then(|| Arc::new(tokio::sync::Semaphore::new(cap)));

    for (index, item_body) in items.into_iter().enumerate() {
        let state = Arc::clone(&state);
        let headers = headers.clone();
        let tx = tx.clone();
        let semaphore = semaphore.clone();
        tokio::spawn(async move {
            // The browser closes the stream when it navigates away. Do not keep
            // resolving and querying charts for a receiver that no longer exists.
            if tx.is_closed() {
                return;
            }
            let _permit = match semaphore.as_ref() {
                Some(semaphore) => Some(
                    semaphore
                        .acquire()
                        .await
                        .expect("batch concurrency semaphore is never closed"),
                ),
                None => None,
            };
            if tx.is_closed() {
                return;
            }
            let timeout = async {
                match stream_item_timeout() {
                    Some(duration) => tokio::time::sleep(duration).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let result = tokio::select! {
                result = batch_chart_bytes(state, headers, item_body) => result,
                _ = tx.closed() => return,
                _ = timeout => Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "chart timed out".to_string(),
                )),
            };
            let line = match result {
                Ok((status, body)) => {
                    let body_text = std::str::from_utf8(&body).unwrap_or("null");
                    format!("{{\"index\":{index},\"status\":{status},\"body\":{body_text}}}\n")
                }
                Err((status, message)) => {
                    let escaped = serde_json::to_string(&message)
                        .unwrap_or_else(|_| "\"batch task failed\"".to_string());
                    format!(
                        "{{\"index\":{index},\"status\":{},\"error\":{escaped}}}\n",
                        status.as_u16()
                    )
                }
            };
            // A disconnected client closes the receiver; the chart task simply ends.
            let _ = tx.send(Ok(Bytes::from(line))).await;
        });
    }
    drop(tx);

    let stream = ReceiverStream::new(rx);
    axum::response::Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/x-ndjson"),
        )
        .header(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        )
        .header("x-dl-stream", "1")
        .header("x-accel-buffering", "no")
        .body(axum::body::Body::from_stream(stream))
        .map_err(batch_internal_error)
}

/// Internal batch assembly error (io or JSON); never part of the public contract.
#[cold]
fn batch_internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

/// Cold `400` constructor for the metric fast path. `#[cold]` keeps these branches out of the
/// hot layout; the value is identical to the inline tuple it replaces.
#[cold]
#[inline(never)]
fn cold_bad_request(message: &'static str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.to_string())
}

/// Cold `502` constructor for the metric fast path (message is already formatted on the cold
/// path only).
#[cold]
#[inline(never)]
fn cold_bad_gateway(message: String) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, message)
}

/// `DL_BATCH_CONCURRENCY`: per-request cap on concurrently executing chart bodies.
/// Deployment-static, parsed once. `0` disables the cap; values are clamped to the batch
/// entry limit (64). The default keeps a normal dashboard batch fully parallel.
fn batch_concurrency() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("DL_BATCH_CONCURRENCY")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(8)
            .min(64)
    })
}

/// One batch chart from its own JSON bytes: the body is parsed inside the spawned task, so
/// the batch request task only splits spans. Same per-chart unit as `batch_run` used before
/// (dispatch through `ui_run_inner`, serialized envelope + status on success, the handler
/// error pair on failure).
pub(crate) async fn batch_chart_bytes(
    state: Arc<AppState>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<(u16, bytes::Bytes), (StatusCode, String)> {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            std::hint::cold_path();
            return Err((
                StatusCode::BAD_REQUEST,
                format!("batch chart body: {error}"),
            ));
        }
    };
    match ui_run_inner(axum::extract::State(state), headers, Json(body)).await {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
                .await
                .map_err(batch_internal_error)?;
            Ok((status, body))
        }
        Err((status, message)) => Err((status, message)),
    }
}

/// Zero-copy split of a batch request body into per-chart JSON spans.
///
/// Accepts `[body, ...]` and `{"requests": [body, ...]}` (the two shapes `batch_run` has
/// always accepted). The caller validates the whole body as JSON first, so this scanner only
/// locates value boundaries; it never interprets the values. Returns `None` when the shape is
/// not a batch (the caller then emits the historical "batch must be an array or
/// {requests:[...]}" 400).
fn split_batch_requests(body: &bytes::Bytes) -> Option<Vec<bytes::Bytes>> {
    let bytes = &body[..];
    let start = skip_json_ws(bytes, 0);
    match bytes.get(start)? {
        b'[' => {
            let end = scan_json_value_end(bytes, start)?;
            split_json_array(body, start, end)
        }
        b'{' => {
            let mut index = start + 1;
            loop {
                let key_start = skip_json_ws(bytes, index);
                if bytes.get(key_start) == Some(&b'}') {
                    return None;
                }
                let key_end = scan_json_string_end(bytes, key_start)?;
                let after_key = skip_json_ws(bytes, key_end);
                if bytes.get(after_key) != Some(&b':') {
                    return None;
                }
                let value_start = skip_json_ws(bytes, after_key + 1);
                let value_end = scan_json_value_end(bytes, value_start)?;
                if &bytes[key_start..key_end] == b"\"requests\"" {
                    if bytes.get(value_start) != Some(&b'[') {
                        return None;
                    }
                    return split_json_array(body, value_start, value_end);
                }
                let after_value = skip_json_ws(bytes, value_end);
                match bytes.get(after_value) {
                    Some(b',') => index = after_value + 1,
                    Some(b'}') => return None,
                    _ => return None,
                }
            }
        }
        _ => None,
    }
}

fn split_json_array(
    body: &bytes::Bytes,
    start: usize,
    end: usize,
) -> Option<Vec<bytes::Bytes>> {
    let bytes = &body[..];
    if bytes.get(start) != Some(&b'[') || bytes.get(end.checked_sub(1)?) != Some(&b']') {
        return None;
    }
    let mut items = Vec::new();
    let mut index = skip_json_ws(bytes, start + 1);
    if bytes.get(index) == Some(&b']') {
        return Some(items);
    }
    loop {
        let value_end = scan_json_value_end(bytes, index)?;
        items.push(body.slice(index..value_end));
        index = skip_json_ws(bytes, value_end);
        match bytes.get(index) {
            Some(b',') => index = skip_json_ws(bytes, index + 1),
            Some(b']') => return Some(items),
            _ => return None,
        }
    }
}

fn skip_json_ws(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        index += 1;
    }
    index
}

/// End (exclusive) of the JSON string starting at `start` (`bytes[start] == b'"'`).
fn scan_json_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut index = start + 1;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => return Some(index + 1),
            b'\\' => index += 2,
            _ => index += 1,
        }
    }
    None
}

/// End (exclusive) of the JSON value starting at `start` (leading whitespace already skipped).
fn scan_json_value_end(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'"' => scan_json_string_end(bytes, start),
        b'{' => {
            let mut index = start + 1;
            loop {
                let key_start = skip_json_ws(bytes, index);
                if bytes.get(key_start) == Some(&b'}') {
                    return Some(key_start + 1);
                }
                let key_end = scan_json_string_end(bytes, key_start)?;
                let after_key = skip_json_ws(bytes, key_end);
                if bytes.get(after_key) != Some(&b':') {
                    return None;
                }
                let value_end = scan_json_value_end(bytes, skip_json_ws(bytes, after_key + 1))?;
                let after_value = skip_json_ws(bytes, value_end);
                match bytes.get(after_value) {
                    Some(b',') => index = after_value + 1,
                    Some(b'}') => return Some(after_value + 1),
                    _ => return None,
                }
            }
        }
        b'[' => {
            let mut index = start + 1;
            loop {
                let value_start = skip_json_ws(bytes, index);
                if bytes.get(value_start) == Some(&b']') {
                    return Some(value_start + 1);
                }
                let value_end = scan_json_value_end(bytes, value_start)?;
                let after_value = skip_json_ws(bytes, value_end);
                match bytes.get(after_value) {
                    Some(b',') => index = after_value + 1,
                    Some(b']') => return Some(after_value + 1),
                    _ => return None,
                }
            }
        }
        b'-' | b'0'..=b'9' => {
            let mut index = start;
            while matches!(
                bytes.get(index),
                Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
            ) {
                index += 1;
            }
            Some(index)
        }
        b't' => bytes
            .get(start..start + 4)
            .filter(|literal| *literal == b"true")
            .map(|_| start + 4),
        b'f' => bytes
            .get(start..start + 5)
            .filter(|literal| *literal == b"false")
            .map(|_| start + 5),
        b'n' => bytes
            .get(start..start + 4)
            .filter(|literal| *literal == b"null")
            .map(|_| start + 4),
        _ => None,
    }
}


#[cfg(test)]
mod metric_typed_path_tests {
    use super::*;

    fn response() -> dl_api::ResultResponse {
        serde_json::from_value(json!({
            "fields": [
                {
                    "legend_item_id": 0,
                    "id": "dim-1",
                    "title": "FirstOrder_Month",
                    "data_type": "date",
                    "field_type": "DIMENSION",
                    "item_type": "field",
                    "role_spec": {"role": "row"}
                },
                {
                    "legend_item_id": 1,
                    "id": "f_sales",
                    "title": "Sales",
                    "data_type": "integer",
                    "field_type": "MEASURE",
                    "item_type": "field",
                    "role_spec": {"role": "measure"}
                }
            ],
            "result_data": [{"rows": [
                {"data": ["2019-05-01", "7"], "legend": [0, 1]}
            ]}],
            "blocks": [],
            "notifications": [],
            "data_export": {
                "background": {"allowed": false, "reason": ["PROHIBITED_EXPORT_TENANT"]},
                "basic": {"allowed": true, "reason": null}
            },
            "data_export_forbidden": false
        }))
        .expect("result response")
    }

    #[test]
    fn typed_metric_data_equals_the_value_path() {
        let typed =
            crate::typed_data::DataBody::Result(crate::typed_data::ResultBody::new(response()));
        let formatting: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"format": "number", "precision": 1})).unwrap();
        let typed_value = metric_typed_data_point(
            &typed,
            "f_sales",
            Some(&formatting),
            "integer",
            "42px",
            "#3D97F2",
            "Sales",
        );
        let expected = metric_data_point(
            Some(&json!(7)),
            Some(&formatting),
            "integer",
            "42px",
            "#3D97F2",
            "Sales",
        );
        assert_eq!(typed_value, expected);
    }

    #[test]
    fn typed_metric_without_rows_returns_empty_object() {
        let mut empty = response();
        empty.result_data.clear();
        let typed =
            crate::typed_data::DataBody::Result(crate::typed_data::ResultBody::new(empty));
        assert_eq!(
            metric_typed_data_point(&typed, "f_sales", None, "integer", "", "", ""),
            json!({})
        );
    }
}


#[cfg(test)]
mod table_typed_parts_tests {
    use super::*;
    use crate::typed_data::{DataBody, ResultBody};

    fn response() -> dl_api::ResultResponse {
        serde_json::from_value(json!({
            "fields": [
                {
                    "legend_item_id": 0,
                    "id": "dim-1",
                    "title": "FirstOrder_Month",
                    "data_type": "date",
                    "field_type": "DIMENSION",
                    "item_type": "field",
                    "role_spec": {"role": "row"}
                },
                {
                    "legend_item_id": 1,
                    "id": "f_sales",
                    "title": "Sales",
                    "data_type": "integer",
                    "field_type": "MEASURE",
                    "item_type": "field",
                    "role_spec": {"role": "measure"}
                }
            ],
            "result_data": [{"rows": [
                {"data": ["2019-05-01", "7"], "legend": [0, 1]},
                {"data": ["2019-06-01", "9"], "legend": [0, 1]}
            ]}],
            "blocks": [],
            "notifications": [],
            "data_export": {
                "background": {"allowed": false, "reason": ["PROHIBITED_EXPORT_TENANT"]},
                "basic": {"allowed": true, "reason": null}
            },
            "data_export_forbidden": false
        }))
        .expect("result response")
    }

    #[test]
    fn fields_body_size_matches_the_serialized_wrapper() {
        let fields = serde_json::json!([
            {"guid": "f1", "title": "Region \"quoted\"", "type": "DIMENSION", "data_type": "string"},
            {"guid": "f2", "title": "Москва 🙂", "type": "MEASURE", "data_type": "float"}
        ]);
        let revision = serde_json::json!("y8dbeddb940mh");
        let body = serde_json::json!({"fields": fields.clone(), "revision_id": revision.clone()});
        assert_eq!(
            fields_body_json_len(&fields, &revision),
            serde_json::to_string(&body).expect("serialize").len() as u64
        );
        // A null revision (the control can run before /fields answers) counts as `null`.
        let revision = Value::Null;
        let body = serde_json::json!({"fields": fields.clone(), "revision_id": revision});
        assert_eq!(
            fields_body_json_len(&fields, &revision),
            serde_json::to_string(&body).expect("serialize").len() as u64
        );
    }

    #[test]
    fn control_dataset_ids_come_from_the_inline_source() {
        let body = serde_json::json!({
            "config": {"data": {"shared": {"sourceType": "dataset",
                "source": {"datasetId": "dataset-1"}}}}
        });
        assert_eq!(control_dataset_ids(&body), vec!["dataset-1".to_string()]);
        assert!(control_dataset_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn typed_graph_wire_value_matches_to_value() {
        let response = response();
        let expected = serde_json::to_value(&response).expect("value");
        let export = serde_json::to_value(&response.data_export).expect("export");
        let (body, returned_export) = DataBody::Result(ResultBody::new(response))
            .into_result_wire_value(export.clone())
            .expect("result wire value");
        assert_eq!(body, expected);
        assert_eq!(returned_export, export);
        assert_eq!(returned_export, expected["data_export"]);
    }

    #[test]
    fn typed_table_parts_match_the_value_body() {
        let response = response();
        let value = serde_json::to_value(&response).expect("value");
        let export = serde_json::to_value(&response.data_export).expect("export");
        let (fields_value, rows_value, export_value) = take_table_value_parts(value);
        let typed = DataBody::Result(ResultBody::new(response))
            .into_table_parts(export)
            .expect("result parts");
        assert_eq!(typed.fields, fields_value);
        assert_eq!(typed.rows, rows_value);
        assert_eq!(typed.data_export, export_value);
    }
}


#[cfg(test)]
mod gravity_v9_tests {
    use super::*;
    use crate::graph_engine::{GraphFetched, GraphPrepared};

    #[test]
    fn gravity_classic_mapping() {
        assert_eq!(classic_gravity_id("bar"), Some("column"));
        assert_eq!(classic_gravity_id("bar100p"), Some("column100p"));
        assert_eq!(classic_gravity_id("scatter"), Some("line"));
        assert_eq!(classic_gravity_id("donut"), Some("pie"));
        assert_eq!(classic_gravity_id("treemap"), Some("column"));
        assert_eq!(classic_gravity_id("line"), None);
    }

    #[test]
    fn minimal_highcharts_envelope_is_non_empty() {
        let prepared = GraphPrepared {
            config: json!({}),
            params: json!({}),
            requests: Vec::new(),
            drill_down: Value::Null,
        };
        let fetched = vec![GraphFetched {
            key: "demo_result".to_string(),
            dataset: "demo".to_string(),
            status: 200,
            latency: 0.0,
            size: 0,
            body: json!({
                "fields": [
                    {"legend_item_id": 0, "id": "f_region", "title": "Region", "data_type": "string"},
                    {"legend_item_id": 1, "id": "f_sales", "title": "Sales", "data_type": "integer"},
                ],
                "result_data": [{"rows": [{"data": ["East", "100"], "legend": [0, 1]}]}],
            }),
            data_export: None,
        }];
        let finished = minimal_highcharts_finished(&prepared, &fetched, "column");
        assert!(!finished.library.as_object().map(|map| map.is_empty()).unwrap_or(true));
        assert_eq!(
            finished.library.pointer("/series").and_then(Value::as_array).map(Vec::len),
            Some(1)
        );
        assert!(finished.data.is_object());
    }

    #[test]
    fn parsed_shared_cache_is_revision_keyed() {
        let text = r#"{"visualization":{"id":"flatTable"},"extraSettings":{}}"#;
        let a = parsed_shared_cached("entry-a", "published", "rev-1", text).expect("parse");
        let b = parsed_shared_cached("entry-a", "published", "rev-1", text).expect("parse");
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        let c = parsed_shared_cached("entry-a", "published", "rev-2", text).expect("parse");
        assert!(!std::sync::Arc::ptr_eq(&a, &c));
        assert!(parsed_shared_cached("entry-bad", "published", "rev-1", "{").is_none());
    }

    #[test]
    fn schema_field_lookup_uses_dataset_and_saved_fields() {
        let dataset = json!([{"guid": "g1", "title": "First", "data_type": "string"}]);
        let shared = json!({"datasetsPartialFields": [[{"guid": "g2", "title": "Second", "data_type": "date"}]]});
        assert_eq!(
            find_schema_field(&dataset, &shared, "g1").and_then(|f| f.get("title").and_then(Value::as_str)),
            Some("First")
        );
        assert_eq!(
            find_schema_field(&dataset, &shared, "Second").and_then(|f| f.get("guid").and_then(Value::as_str)),
            Some("g2")
        );
        assert!(find_schema_field(&dataset, &shared, "missing").is_none());
    }

    #[test]
    fn table_request_cache_isolates_params_and_revision() {
        let shared = json!({
            "visualization": {
                "id": "flatTable",
                "placeholders": [{"items": [
                    {"guid": "g1", "title": "First", "data_type": "string", "type": "DIMENSION"}
                ]}]
            },
            "extraSettings": {}
        });
        let revision = json!("ds-rev-1");
        let fields = json!([{"guid": "g1", "title": "First", "data_type": "string"}]);
        let params_a = json!({"g1": "a"});
        let params_b = json!({"g1": "b"});
        let meta_id = json!("chart-1");
        let meta_rev = json!("rev-1");
        let a1 = table_request_cached(
            &shared, &params_a, "ds", &revision, &fields, None, "published", &meta_id, &meta_rev, "en",
        );
        let a2 = table_request_cached(
            &shared, &params_a, "ds", &revision, &fields, None, "published", &meta_id, &meta_rev, "en",
        );
        let b = table_request_cached(
            &shared, &params_b, "ds", &revision, &fields, None, "published", &meta_id, &meta_rev, "en",
        );
        let other_rev = table_request_cached(
            &shared, &params_a, "ds", &revision, &fields, None, "published", &meta_id, &json!("rev-2"), "en",
        );
        assert!(std::sync::Arc::ptr_eq(&a1, &a2));
        assert!(!std::sync::Arc::ptr_eq(&a1, &b));
        assert!(!std::sync::Arc::ptr_eq(&a1, &other_rev));
        assert!(a1.is_object());
    }

}
