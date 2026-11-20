//! Node/pino-compatible structured logging plus the HTTP response headers the
//! Node service adds on every response.
//!
//! Node writes one JSON object per line:
//!
//! ```json
//! {"level":30,"time":1793747882527,"pid":1,"hostname":"ba21f6f6f6c8",
//!  "name":"united-storage","req":{"id":"…","method":"GET","url":"/v1/entries/…",
//!  "headers":{…},"remoteAddress":"::ffff:172.18.0.9","remotePort":34238},
//!  "msg":"[Express GET] Request started […id]"}
//! ```
//!
//! The same shape is emitted here for the request lifecycle
//! (start / `REQUEST_INFO` / controller events / completed) and for the service's
//! own tracing events. On the response side the Node service always adds
//! `x-request-id`, `surrogate-control` and `cache-control` (JSON responses),
//! and `Content-Type: application/json; charset=utf-8`.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::state::AppState;

/// `name` field of every log record (`nodekit` uses the app name).
const LOG_NAME: &str = "united-storage";

const CACHE_CONTROL: &str = "no-store, max-age=0, must-revalidate, proxy-revalidate";

/// Monotonic part of generated request ids.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-request data shared by the lifecycle/event logs.
#[derive(Clone)]
pub struct RequestLog {
    pub id: String,
    pub method: String,
    pub url: String,
    /// Full `req` (headers, remote address) as logged on "Request started".
    req_json_full: String,
    /// `req` without headers, as pino's bound child logger logs it on every
    /// subsequent line (`{id, method, url}`).
    req_json_short: String,
}

impl RequestLog {
    fn req_json(&self, full: bool) -> &str {
        if full {
            &self.req_json_full
        } else {
            &self.req_json_short
        }
    }
}

/// Writes one pino-shaped record to stdout.
/// Controller-level event (Node: `ctx.log` in services/actions).
pub fn event(request: &RequestLog, scope: &str, message: &str, fields: Map<String, Value>) {
    let msg = format!(
        "[Express {}] [{scope}] {message} [{}]",
        request.method, request.id
    );
    write_record(30, &msg, Some(request), fields);
}

pub fn write_record(
    level: u8,
    msg: &str,
    request: Option<&RequestLog>,
    fields: Map<String, Value>,
) {
    // One `write_all` per record: the record is assembled in a reused buffer
    // instead of allocating a `serde_json::Value` tree.
    let mut line = String::with_capacity(256);
    line.push_str("{\"level\":");
    line.push_str(&level.to_string());
    line.push_str(",\"time\":");
    line.push_str(&now_ms().to_string());
    line.push_str(",\"pid\":");
    line.push_str(&std::process::id().to_string());
    line.push_str(",\"hostname\":");
    push_json_string(&mut line, hostname());
    line.push_str(",\"name\":");
    push_json_string(&mut line, LOG_NAME);

    if let Some(request) = request {
        line.push_str(",\"req\":");
        line.push_str(request.req_json(false));
    }

    for (key, value) in fields {
        line.push(',');
        push_json_string(&mut line, &key);
        line.push(':');
        if let Ok(serialized) = serde_json::to_string(&value) {
            line.push_str(&serialized);
        } else {
            line.push_str("null");
        }
    }

    line.push_str(",\"msg\":");
    push_json_string(&mut line, msg);
    line.push('}');
    line.push('\n');

    // A dedicated writer thread keeps stdout I/O off the request path; a
    // `BufWriter` turns the five lines of a request into one syscall burst.
    if log_sender().send(line).is_err() {
        eprintln!("us: log channel is closed, dropping a log line");
    }
}

fn log_sender() -> &'static mpsc::Sender<String> {
    static SENDER: OnceLock<mpsc::Sender<String>> = OnceLock::new();

    SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("us-rs-log".to_string())
            .spawn(move || {
                let mut buffer = String::with_capacity(64 * 1024);
                while let Ok(line) = receiver.recv() {
                    buffer.push_str(&line);
                    // Drain everything that is already queued, then write the
                    // batch with a short-lived stdout lock. The lock is *not*
                    // held between batches: tracing events (e.g. sqlx query
                    // logs) also write to stdout and must not block forever.
                    loop {
                        match receiver.try_recv() {
                            Ok(next) => buffer.push_str(&next),
                            Err(_) => break,
                        }
                    }
                    let stdout = std::io::stdout();
                    let mut lock = stdout.lock();
                    let _ = lock.write_all(buffer.as_bytes());
                    let _ = lock.flush();
                    drop(lock);
                    buffer.clear();
                }
            })
            .expect("spawn log writer");
        sender
    })
}

/// Fast path: `extra` is a pre-serialized JSON object fragment without the
/// leading comma (e.g. `"res":{...}`), so no `serde_json::Value` is allocated.
fn write_record_extra(level: u8, msg: &str, request: &RequestLog, extra: &str) {
    write_record_extra_with(level, msg, request, extra, false)
}

fn write_record_extra_with(
    level: u8,
    msg: &str,
    request: &RequestLog,
    extra: &str,
    full_req: bool,
) {
    let mut line = String::with_capacity(256 + extra.len());
    line.push_str("{\"level\":");
    line.push_str(&level.to_string());
    line.push_str(",\"time\":");
    line.push_str(&now_ms().to_string());
    line.push_str(",\"pid\":");
    line.push_str(&std::process::id().to_string());
    line.push_str(",\"hostname\":");
    push_json_string(&mut line, hostname());
    line.push_str(",\"name\":");
    push_json_string(&mut line, LOG_NAME);
    line.push_str(",\"req\":");
    line.push_str(request.req_json(full_req));
    if !extra.is_empty() {
        line.push(',');
        line.push_str(extra);
    }
    line.push_str(",\"msg\":");
    push_json_string(&mut line, msg);
    line.push_str("}\n");
    if log_sender().send(line).is_err() {
        eprintln!("us: log channel is closed, dropping a log line");
    }
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn hostname() -> &'static str {
    static HOSTNAME: OnceLock<String> = OnceLock::new();

    HOSTNAME.get_or_init(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "localhost".to_string())
    })
}

fn xorshift(value: &mut u64) -> u64 {
    let mut x = *value;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *value = x;
    x
}

/// UUID v4-shaped id. Node's `expresskit` generates a UUID when the caller did
/// not pass `x-request-id`; the gateway always passes one (`dl.<pid>.<hex>`).
fn generate_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state =
        nanos ^ (counter.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ ((std::process::id() as u64) << 32);

    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let value = xorshift(&mut state).to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Headers Node redacts in logs: nodekit defaults + the US master tokens
/// (`appSensitiveHeaders`). Logging them in clear text would leak credentials.
const SENSITIVE_HEADERS: [&str; 6] = [
    "authorization",
    "cookie",
    "set-cookie",
    "password",
    "x-us-master-token",
    "x-us-dynamic-master-token",
];

fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            let value = if SENSITIVE_HEADERS.contains(&name.as_str()) {
                "[REDACTED]".to_string()
            } else {
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            };
            (name, value)
        })
        .collect()
}

fn remote_info(request: &Request, trust_proxy: u32) -> (String, u16) {
    let (socket_ip, port) = match request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
    {
        Some(ConnectInfo(address)) => {
            let ip = match address.ip() {
                std::net::IpAddr::V4(v4) => format!("::ffff:{v4}"),
                std::net::IpAddr::V6(v6) => v6.to_string(),
            };
            (ip, address.port())
        }
        None => ("::ffff:127.0.0.1".to_string(), 0),
    };

    // express `trust proxy` (number): `req.ip` is the first address in the
    // X-Forwarded-For chain that is not a trusted proxy. With N trusted hops
    // the client sits at `xff.len() - N`.
    if trust_proxy > 0 {
        if let Some(value) = request.headers().get("x-forwarded-for") {
            if let Ok(value) = value.to_str() {
                let chain: Vec<&str> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect();

                if !chain.is_empty() {
                    // express reverses the chain (closest proxy first) and skips
                    // `trust_proxy` trusted hops, so the client is the first
                    // entry when the chain is longer than the trust count.
                    let index = chain.len().saturating_sub(1 + trust_proxy as usize);
                    let client = chain.get(index).copied().unwrap_or(chain[0]);
                    return (client.to_string(), port);
                }
            }
        }
    }

    (socket_ip, port)
}

/// Controller scope (the `[getEntry]` part of the message) and the two events
/// the Node controller logs around its work.
struct RouteEvents {
    scope: &'static str,
    start: &'static str,
    finish: &'static str,
}

fn route_events(method: &Method, matched_path: &str) -> Option<RouteEvents> {
    let path = matched_path.trim_end_matches('/');
    let get = method == Method::GET;
    let post = method == Method::POST;

    let entry = |scope, start, finish| {
        Some(RouteEvents {
            scope,
            start,
            finish,
        })
    };

    if get && path == "/v1/entries" {
        return entry(
            "getEntries",
            "GET_ENTRIES_REQUEST",
            "GET_ENTRIES_REQUEST_SUCCESS",
        );
    }
    if (post && (path == "/v1/get-entries" || path == "/private/v1/get-entries")) {
        return entry(
            "getEntriesV2",
            "GET_ENTRIES_REQUEST",
            "GET_ENTRIES_REQUEST_SUCCESS",
        );
    }
    if get && path == "/v2/workbooks" {
        return entry(
            "getWorkbooksListController",
            "GET_WORKBOOKS_LIST_START",
            "GET_WORKBOOKS_LIST_FINISH",
        );
    }
    if get && path == "/v1/structure-items" {
        return entry(
            "getStructureItemsController",
            "GET_STRUCTURE_ITEMS_START",
            "GET_STRUCTURE_ITEMS_FINISH",
        );
    }
    if get && path == "/v1/collection-content" {
        return entry(
            "getCollectionContentController",
            "GET_COLLECTION_CONTENT_START",
            "GET_COLLECTION_CONTENT_FINISH",
        );
    }
    if get && path == "/v1/root-collection-permissions" {
        return entry(
            "getRootPermissionsController",
            "GET_ROOT_COLLECTION_PERMISSIONS_START",
            "GET_ROOT_COLLECTION_PERMISSIONS_FINISH",
        );
    }
    if get && path == "/v1/entries/{entry_id}" || (get && path == "/private/entries/{entry_id}") {
        return entry("getEntry", "GET_ENTRY_REQUEST", "GET_ENTRY_SUCCESS");
    }
    if get && (path == "/v1/entries/{entry_id}/meta" || path == "/private/entries/{entry_id}/meta")
    {
        return entry(
            "getEntryMetaController",
            "GET_ENTRY_META_REQUEST",
            "GET_ENTRY_META_SUCCESS",
        );
    }
    if get
        && (path == "/v1/entries/{entry_id}/revisions"
            || path == "/private/entries/{entry_id}/revisions")
    {
        return entry(
            "getRevisionsController",
            "GET_REVISIONS_REQUEST",
            "GET_REVISIONS_SUCCESS",
        );
    }
    if get
        && (path == "/v1/entries/{entry_id}/relations"
            || path == "/private/entries/{entry_id}/relations")
    {
        return entry(
            "getRelationsController",
            "GET_ENTRY_RELATIONS_REQUEST",
            "GET_ENTRY_RELATIONS_SUCCESS",
        );
    }
    if get && path == "/v1/entries/{entry_id}/access-description" {
        return entry(
            "getEntryAccessDescription",
            "GET_ENTRY_ACCESS_DESCRIPTION_REQUEST",
            "GET_ENTRY_ACCESS_DESCRIPTION_SUCCESS",
        );
    }
    if get && path == "/v2/workbooks/{workbook_id}/entries" {
        return entry(
            "getWorkbookContentController",
            "GET_WORKBOOK_CONTENT_START",
            "GET_WORKBOOK_CONTENT_FINISH",
        );
    }
    if get && path == "/v1/favorites" {
        return entry(
            "getFavoritesController",
            "GET_FAVORITES_REQUEST",
            "GET_FAVORITES_SUCCESS",
        );
    }
    if get && path == "/v1/tenants/details" {
        return entry(
            "getTenantDetailsController",
            "GET_TENANT_REQUEST",
            "GET_TENANT_SUCCESS",
        );
    }
    if get && path == "/v1/color-palettes" {
        return entry(
            "getColorPalettesListController",
            "GET_COLOR_PALETTES_LIST_START",
            "GET_COLOR_PALETTES_LIST_FINISH",
        );
    }
    if post && path == "/v1/check-entries-existence" {
        return entry(
            "checkEntriesExistenceController",
            "CHECK_ENTRIES_EXISTENCE_REQUEST",
            "CHECK_ENTRIES_EXISTENCE_SUCCESS",
        );
    }
    if post && path == "/v1/get-entries-data" {
        return entry(
            "getEntriesDataController",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_REQUEST",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_SUCCESS",
        );
    }
    if post && path == "/v1/get-entries-meta" {
        return entry(
            "getEntriesMetaController",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_REQUEST",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_SUCCESS",
        );
    }
    if post && path == "/v1/get-entries-annotation" {
        return entry(
            "getEntriesAnnotationController",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_REQUEST",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_SUCCESS",
        );
    }
    if post && path == "/v1/get-entries-relations" {
        return entry(
            "getEntriesRelationsController",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_REQUEST",
            "GET_JOINED_ENTRIES_REVISIONS_BY_IDS_SUCCESS",
        );
    }

    None
}

fn event_message(method: &str, scope: Option<&str>, event: &str, id: &str) -> String {
    match scope {
        Some(scope) => format!("[Express {method}] [{scope}] {event} [{id}]"),
        None => format!("[Express {method}] {event} [{id}]"),
    }
}

/// `POSTGRES_DSN_LIST`-independent middleware: request id, Node headers, pino
/// lifecycle logs and the controller start/finish events.
pub async fn middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let matched_path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|path| path.as_str().to_string())
        .unwrap_or_default();

    let incoming_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let id = incoming_id.unwrap_or_else(generate_request_id);
    let method = request.method().clone();
    let url = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let logging = state.config.http_log;
    let log = if logging {
        let headers = collect_headers(request.headers());
        let (remote_address, remote_port) = remote_info(&request, state.config.trust_proxy);

        let mut req_headers = Map::new();
        for (name, value) in headers {
            req_headers.insert(name, Value::String(value));
        }
        let req_json_full = serde_json::to_string(&json!({
            "id": id,
            "method": method.as_str(),
            "url": url,
            "headers": req_headers,
            "remoteAddress": remote_address,
            "remotePort": remote_port,
        }))
        .unwrap_or_else(|_| "{}".to_string());
        let req_json_short = serde_json::to_string(&json!({
            "id": id,
            "method": method.as_str(),
            "url": url,
        }))
        .unwrap_or_else(|_| "{}".to_string());

        Some(RequestLog {
            id: id.clone(),
            method: method.as_str().to_string(),
            url,
            req_json_full,
            req_json_short,
        })
    } else {
        None
    };

    let started = std::time::Instant::now();

    if let Some(log) = &log {
        write_record_extra_with(
            30,
            &format!("[Express {}] Request started [{id}]", method),
            log,
            "",
            true,
        );
    }

    // `ctx` middleware: the tenant and the effective user.
    if let Some(log) = &log {
        let mut ctx = String::from("\"ctxTenantId\":");
        push_json_string(&mut ctx, &state.config.tenant_id_override);
        ctx.push_str(",\"requestedBy\":{\"userId\":\"systemId\",\"login\":\"unknown\"}");
        ctx.push_str(",\"dlContext\":\"{}\"");
        write_record_extra(
            30,
            &format!("[Express {}] [ctx middleware] REQUEST_INFO [{id}]", method),
            log,
            &ctx,
        );
    }

    let events = route_events(&method, &matched_path);
    if let (Some(events), Some(log)) = (&events, &log) {
        write_record(
            30,
            &event_message(method.as_str(), Some(events.scope), events.start, &id),
            Some(log),
            Map::new(),
        );
    }

    if let Some(log) = &log {
        request.extensions_mut().insert(log.clone());
    }

    let mut response = next.run(request).await;

    if let (Some(events), Some(log)) = (&events, &log) {
        if response.status().is_success() {
            write_record(
                30,
                &event_message(method.as_str(), Some(events.scope), events.finish, &id),
                Some(log),
                Map::new(),
            );
        }
    }

    // `error-response-presenter`: after `context failed` Node logs the thrown
    // error message as an info line without a scope. Only error bodies are
    // buffered (they are tiny), so the success path stays zero-copy.
    if response.status().is_client_error() || response.status().is_server_error() {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 1024 * 1024)
            .await
            .unwrap_or_default();
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });

        if let (Some(message), Some(log)) = (message, &log) {
            // Node logs the thrown error twice: `err` at level 50 on
            // "context failed", then `error` on the presenter line.
            let mut fields = Map::new();
            fields.insert(
                "err".to_string(),
                json!({"type": "Error", "message": message}),
            );
            if let Some(events) = &events {
                write_record(
                    50,
                    &event_message(method.as_str(), Some(events.scope), "context failed", &id),
                    Some(log),
                    fields,
                );
            }

            let mut fields = Map::new();
            fields.insert(
                "error".to_string(),
                json!({"type": "UsValidationError", "message": message}),
            );
            write_record(
                30,
                &format!("[Express {}] {message} [{id}]", method),
                Some(log),
                fields,
            );
        }

        response = Response::from_parts(parts, axum::body::Body::from(bytes));
    }

    // Response headers the Node service always sets.
    let headers_mut = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&id) {
        headers_mut.insert(HeaderName::from_static("x-request-id"), value);
    }

    let is_json = headers_mut
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false);

    if is_json {
        headers_mut.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers_mut.insert(
            HeaderName::from_static("surrogate-control"),
            HeaderValue::from_static("no-store"),
        );
        headers_mut.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL),
        );
    }

    let mut response_headers: Vec<(String, String)> = Vec::with_capacity(5);
    for name in [
        "x-request-id",
        "surrogate-control",
        "cache-control",
        "content-type",
    ] {
        if let Some(value) = response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
        {
            response_headers.push((name.to_string(), value.to_string()));
        }
    }

    // `Json`/`String` bodies have an exact size hint, so no buffering is needed.
    let content_length = {
        use http_body::Body as _;
        response
            .body()
            .size_hint()
            .exact()
            .map(|length| length as usize)
    };
    if let Some(length) = content_length {
        if is_json || !response.headers().contains_key(header::CONTENT_LENGTH) {
            response_headers.push(("content-length".to_string(), length.to_string()));
            if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
                response.headers_mut().insert(header::CONTENT_LENGTH, value);
            }
        }
    }

    let status = response.status();
    let response_time = started.elapsed().as_millis() as u64;

    let mut res = String::from("\"res\":{\"responseTime\":");
    res.push_str(&response_time.to_string());
    res.push_str(",\"statusCode\":");
    push_json_string(&mut res, status.as_str());
    res.push_str(",\"headers\":{");
    let mut first = true;
    for (name, value) in &response_headers {
        if !first {
            res.push(',');
        }
        first = false;
        push_json_string(&mut res, name);
        res.push(':');
        push_json_string(&mut res, value);
    }
    res.push_str("}}");
    if let Some(log) = &log {
        write_record_extra(
            30,
            &format!("[Express {}] Request completed [{id}]", method),
            log,
            &res,
        );
    }

    response
}

/// Fields used by handler-level events (kept for future handler instrumentation).
#[allow(dead_code)]
pub fn log_event(
    level: u8,
    request: &RequestLog,
    method: &str,
    scope: &str,
    event: &str,
    fields: Map<String, Value>,
) {
    let id = request.id.clone();
    write_record(
        level,
        &event_message(method, Some(scope), event, &id),
        Some(request),
        fields,
    );
}

// ---------------------------------------------------------------------------
// tracing subscriber in pino format (startup lines and any remaining events)
// ---------------------------------------------------------------------------

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

struct FieldVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value));
        }
    }
}

struct PinoFormat;

impl<S, N> FormatEvent<S, N> for PinoFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => 50,
            tracing::Level::WARN => 40,
            tracing::Level::INFO => 30,
            tracing::Level::DEBUG => 20,
            tracing::Level::TRACE => 10,
        };

        let mut visitor = FieldVisitor {
            message: None,
            fields: Map::new(),
        };
        event.record(&mut visitor);

        let mut record = Map::new();
        record.insert("level".to_string(), Value::from(level));
        record.insert("time".to_string(), Value::from(now_ms()));
        record.insert("pid".to_string(), Value::from(std::process::id()));
        record.insert(
            "hostname".to_string(),
            Value::String(hostname().to_string()),
        );
        record.insert("name".to_string(), Value::String(LOG_NAME.to_string()));

        for (key, value) in visitor.fields {
            record.insert(key, value);
        }
        record.insert(
            "msg".to_string(),
            Value::String(visitor.message.unwrap_or_default()),
        );

        let line = serde_json::to_string(&Value::Object(record)).map_err(|_| std::fmt::Error)?;
        writer.write_str(&line)?;
        writer.write_char('\n')
    }
}

/// Installs the pino-shaped tracing subscriber.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("US_RS_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,us_rs=debug"));

    let _ = tracing_subscriber::fmt()
        .event_format(PinoFormat)
        .with_env_filter(filter)
        .try_init();
}
