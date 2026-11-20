//! In-process charts-engine HTTP layer.
//!
//! The engine is pure Rust: `/api/run` is dispatched by `crate::pipeline`, the
//! chart builders live in `graph_engine`, `gravity_*`, `table`/`pivot_engine` and
//! `run_metric_rust`. There is no JavaScript runtime in this binary.

use axum::serve::ListenerExt;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::timeout::TimeoutLayer;



/// Compression settings, all from the environment so a deployment can move the
/// trade-off without a rebuild:
///
/// * `DL_COMPRESSION` — `zstd` (default profile: prefer zstd, fall back to gzip when the
///   client only advertises gzip), `gzip` (force gzip; nothing when the client does not
///   accept it) or `off`/`identity`/`none`/`0` (identity — the max-RPS profile).
/// * `DL_ZSTD_LEVEL` — level for ordinary responses (default 3).
/// * `DL_COMPRESS_MIN_CHUNK` — coalesce body chunks smaller than this before flushing.
#[derive(Clone, Copy)]
struct Compression {
    enabled: bool,
    /// `DL_COMPRESSION=gzip`: never use zstd, even when the client accepts it.
    force_gzip: bool,
    level: i32,
    /// Chunks smaller than this are coalesced before a zstd/gzip flush.
    min_chunk: usize,
    /// Lower zstd level for NDJSON streams: the payload is many small frames, so CPU
    /// latency matters more than the last few percent of compression.
    stream_level: i32,
}

// One JWT verification per `/api/run` request: the chart dispatch fans out into 2-3
// in-process data-api calls, and each used to re-validate the same token with RSA-PSS.
async fn ui_run_identity(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::Json<Value>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    crate::memory::touch();
    crate::data_direct::scope_identity(
        &headers,
        crate::pipeline::ui_run(state, headers.clone(), body),
    )
    .await
}

async fn ui_run_raw_identity(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    crate::memory::touch();
    crate::data_direct::scope_identity(
        &headers,
        crate::pipeline::ui_run_raw(state, headers.clone(), body),
    )
    .await
}

async fn batch_run_identity(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    crate::memory::touch();
    crate::data_direct::scope_identity(
        &headers,
        crate::pipeline::batch_run(state, headers.clone(), body),
    )
    .await
}

async fn batch_run_stream_identity(
    state: axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    crate::memory::touch();
    crate::data_direct::scope_identity(
        &headers,
        crate::pipeline::batch_run_stream(state, headers.clone(), body),
    )
    .await
}


/// `/debug/*` is admin-only when auth is enabled (the public port must not expose process
/// internals); the guard lives in `data_direct` so the token validation is the same NATIVE
/// validator the data routes use.
fn debug_error(headers: &HeaderMap) -> Option<Response> {
    crate::data_direct::require_debug_admin(headers)
        .err()
        .map(|(status, message)| (status, message).into_response())
}

async fn debug_cache(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(error) = debug_error(&headers) {
        return error;
    }
    crate::pipeline::cache_stats(State(state)).await.into_response()
}

async fn debug_stages(headers: HeaderMap) -> Response {
    if let Some(error) = debug_error(&headers) {
        return error;
    }
    crate::pipeline::stage_stats().await.into_response()
}

async fn debug_alloc(headers: HeaderMap) -> Response {
    if let Some(error) = debug_error(&headers) {
        return error;
    }
    alloc_stats().await.into_response()
}

async fn debug_alloc_reset(headers: HeaderMap) -> Response {
    if let Some(error) = debug_error(&headers) {
        return error;
    }
    alloc_reset().await.into_response()
}

fn compression_from_env() -> Compression {
    fn number<T: std::str::FromStr>(key: &str, default: T) -> T {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }
    let profile = std::env::var("DL_COMPRESSION")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Compression {
        enabled: !matches!(profile.as_str(), "off" | "identity" | "none" | "0" | "false"),
        force_gzip: profile == "gzip",
        level: number("DL_ZSTD_LEVEL", 3),
        min_chunk: number("DL_COMPRESS_MIN_CHUNK", 4096),
        stream_level: number("DL_ZSTD_STREAM_LEVEL", 1),
    }
}

fn accepts(header: Option<&HeaderValue>, encoding: &str) -> bool {
    header
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains(encoding))
        .unwrap_or(false)
}

/// `DL_RUN_TIMEOUT_SECS` — the outer deadline of one `/api/run*` request.
///
/// In the monolith the engine router is merged into the public app, so `proxy::public_timeout`
/// (which only wraps the fallback dispatcher, `proxy.rs:153`) never sees these routes. Without
/// this layer a hung US read or a query waiting for a database reply held the HTTP request, its
/// connection and a batch permit for as long as the backend wanted. `0` disables the deadline
/// for max-RPS lab runs; an unparseable value keeps the default.
fn run_timeout() -> Option<Duration> {
    static TIMEOUT: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| parse_run_timeout(std::env::var("DL_RUN_TIMEOUT_SECS").ok().as_deref()))
}

fn parse_run_timeout(raw: Option<&str>) -> Option<Duration> {
    match raw.and_then(|value| value.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds)),
        None => Some(Duration::from_secs(120)),
    }
}

/// Applies [`run_timeout`]. Separate from `run()` so the wiring is testable without building
/// the whole `AppState`.
fn with_run_timeout(router: Router, timeout: Option<Duration>) -> Router {
    match timeout {
        Some(timeout) => router.layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            timeout,
        )),
        None => router,
    }
}

/// zstd (or gzip) around the handler's body, with the level chosen by the body
/// size. `content-length` is dropped because the body is re-encoded.
async fn compress(
    Extension(settings): Extension<Compression>,
    request: Request,
    next: Next,
) -> Response {
    let accept = request.headers().get(header::ACCEPT_ENCODING).cloned();
    let zstd = !settings.force_gzip && accepts(accept.as_ref(), "zstd");
    let gzip = accepts(accept.as_ref(), "gzip");
    let response = next.run(request).await;
    if !settings.enabled
        || (!zstd && !gzip)
        || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        let mut response = response;
        // Internal marker only for the compression path; never expose it to clients.
        response.headers_mut().remove("x-dl-stream");
        return response;
    }
    // Streaming responses opt out of the normal chunk coalescing: one NDJSON line
    // must be flushed immediately, while ordinary responses keep the denser default.
    let is_stream = response.headers().contains_key("x-dl-stream");
    let (parts, body) = response.into_parts();
    let (input_tx, input_rx) = mpsc::channel::<Option<Vec<u8>>>(8);
    let (output_tx, output_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let level = if is_stream { settings.stream_level } else { settings.level };
    tokio::task::spawn_blocking(move || {
        let mut encoder = match Encoder::new(zstd, level) {
            Ok(encoder) => encoder,
            Err(_) => return,
        };
        let mut input_rx = input_rx;
        while let Some(Some(chunk)) = input_rx.blocking_recv() {
            if encoder.write_all(&chunk).is_err() || encoder.flush().is_err() {
                return;
            }
            let produced = encoder.take();
            if !produced.is_empty() && output_tx.blocking_send(Ok(Bytes::from(produced))).is_err() {
                return;
            }
        }
        if let Some(tail) = encoder.finish() {
            let _ = output_tx.blocking_send(Ok(Bytes::from(tail)));
        }
    });
    let min_chunk = if is_stream { 1 } else { settings.min_chunk };
    tokio::spawn(async move {
        let mut body = body.into_data_stream();
        // Do not flush a one-byte `{` or every 8 KiB slice as its own zstd frame; coalescing
        // small chunks keeps the compressed stream dense without delaying the first payload.
        let mut pending = Vec::with_capacity(min_chunk.max(1));
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    pending.extend_from_slice(&chunk);
                    if pending.len() >= min_chunk
                        && input_tx
                            .send(Some(std::mem::take(&mut pending)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                }
                Err(error) => {
                    if !pending.is_empty() {
                        let _ = input_tx.send(Some(std::mem::take(&mut pending))).await;
                    }
                    let _ = input_tx.send(None).await;
                    tracing::warn!(%error, "response body read failed during compression");
                    return;
                }
            }
        }
        if !pending.is_empty() {
            let _ = input_tx.send(Some(pending)).await;
        }
        let _ = input_tx.send(None).await;
    });
    let mut response =
        Response::from_parts(parts, Body::from_stream(ReceiverStream::new(output_rx)));
    let headers = response.headers_mut();
    headers.remove("x-dl-stream");
    headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(if zstd { "zstd" } else { "gzip" }),
    );
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    response
}

/// A writer that hides the two encoders behind the few operations the streaming compressor uses.
enum Encoder {
    Zstd(zstd::stream::Encoder<'static, Vec<u8>>),
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
}

impl Encoder {
    fn new(zstd: bool, level: i32) -> std::io::Result<Self> {
        if zstd {
            zstd::stream::Encoder::new(Vec::new(), level)
                .map(Self::Zstd)
                .map_err(std::io::Error::other)
        } else {
            Ok(Self::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::new(level.max(1) as u32),
            )))
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Zstd(encoder) => encoder.write_all(bytes),
            Self::Gzip(encoder) => encoder.write_all(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Zstd(encoder) => encoder.flush(),
            Self::Gzip(encoder) => encoder.flush(),
        }
    }

    fn take(&mut self) -> Vec<u8> {
        match self {
            Self::Zstd(encoder) => std::mem::take(encoder.get_mut()),
            Self::Gzip(encoder) => std::mem::take(encoder.get_mut()),
        }
    }

    fn finish(self) -> Option<Vec<u8>> {
        match self {
            Self::Zstd(encoder) => encoder.finish().ok(),
            Self::Gzip(encoder) => encoder.finish().ok(),
        }
    }
}

#[cfg(feature = "alloc-profile")]
async fn alloc_stats() -> Json<serde_json::Value> {
    Json(crate::alloc_profile::snapshot())
}
#[cfg(not(feature = "alloc-profile"))]
async fn alloc_stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({"error":"alloc-profile feature is disabled"}))
}
#[cfg(feature = "alloc-profile")]
async fn alloc_reset() -> Json<serde_json::Value> {
    crate::alloc_profile::reset();
    Json(serde_json::json!({"reset":true}))
}
#[cfg(not(feature = "alloc-profile"))]
async fn alloc_reset() -> Json<serde_json::Value> {
    Json(serde_json::json!({"error":"alloc-profile feature is disabled"}))
}

#[derive(Clone)]
pub struct AppState {
    pub http: crate::local_http::Client,
    pub data_api: String,
    pub us: String,
    pub us_token: String,
    pub ui: String,
    /// Optional original engine for visualisations the Rust path does not
    /// reproduce yet (combined-chart, geo, QL): the container stays a working
    /// drop-in while those are ported.
    pub node_fallback: Option<String>,
    /// Tenant default palette (opensource config: default20); also the
    /// processor's fallback for extra.colors.
    pub default_color_palette: String,
    /// Short TTL cache for data-api /fields (schema + revision). A dashboard
    /// asks for the same dataset dozens of times; the schema is immutable for
    /// the lifetime of a revision.
    /// `(stored_at, fields, revision_id)` per cache key. `Arc` so a cache hit hands the typed
    /// charts engine the same parsed schema instead of deep-cloning it on every chart.
    pub fields_cache: Arc<std::sync::Mutex<
        std::collections::HashMap<String, (std::time::Instant, Arc<Value>, Arc<Value>)>,
    >>,
    pub fields_flight:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub fields_ttl: std::time::Duration,
    pub fields_max: usize,
    pub auth_enabled: bool,
    pub fields_cache_auth: bool,
    pub tenant: String,
    pub branch: String,
    pub fields_hits: Arc<AtomicU64>,
    pub fields_misses: Arc<AtomicU64>,
    pub fields_fetches: Arc<AtomicU64>,
    pub fields_invalidations: Arc<AtomicU64>,
}

async fn ping() -> &'static str {
    "ok"
}

/// `charts-engine --healthcheck <path>` — probe the local port without a shell/curl.
/// Distroless/scratch images cannot run a CMD-SHELL healthcheck; the binary probes itself.
fn run_healthcheck(path: &str) -> i32 {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let port: u16 = std::env::var("ENGINE_PORT")
        .or_else(|_| std::env::var("PORT"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8089);
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return 1;
    };
    let request = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return 1;
    }
    let mut response = [0u8; 32];
    let Ok(n) = stream.read(&mut response) else {
        return 1;
    };
    let head = String::from_utf8_lossy(&response[..n]);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        0
    } else {
        1
    }
}

/// v8: driven by `main.rs` on the single service runtime (`Handle::block_on` from the
/// charts-engine startup thread). It installs the in-process `ENGINE_ROUTER` and returns;
/// the public app mounts those routes directly.
pub async fn run() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(index) = args.iter().position(|arg| arg == "--healthcheck") {
        let path = args.get(index + 1).map(String::as_str).unwrap_or("/ping");
        std::process::exit(run_healthcheck(path));
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .try_init();
    // Attach the durable workbook-transfer job store before routes are served. If the
    // database is unavailable this falls back to the in-memory store and startup continues.
    let _ = crate::meta_api::init().await;
    let data_api =
        std::env::var("DATA_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let us = std::env::var("US_URL").unwrap_or_else(|_| "http://usr-us:8080".to_string());
    let us_token =
        std::env::var("US_MASTER_TOKEN").unwrap_or_default();
    let port: u16 = std::env::var("ENGINE_PORT")
        .or_else(|_| std::env::var("PORT"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8089);
    let state = Arc::new(AppState {
        http: crate::local_http::Client::new(),
        data_api,
        us,
        us_token,
        ui: std::env::var("UI_URL").unwrap_or_else(|_| "http://ui:8080".to_string()),
        node_fallback: std::env::var("NODE_FALLBACK_URL")
            .ok()
            .filter(|url| !url.is_empty()),
        default_color_palette: std::env::var("DEFAULT_COLOR_PALETTE_ID")
            .unwrap_or_else(|_| "default20".to_string()),
        fields_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        fields_flight: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        fields_ttl: std::time::Duration::from_secs(
            std::env::var("DATASET_FIELDS_TTL_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        ),
        fields_max: std::env::var("DATASET_FIELDS_MAX")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4096),
        // Fail closed: authentication is on unless the deployment explicitly turns it off.
        auth_enabled: std::env::var("AUTH_ENABLED")
            .map(|value| value == "true" || value == "1")
            .unwrap_or(true),
        fields_cache_auth: std::env::var("FIELDS_CACHE_AUTH")
            .map(|value| value == "true" || value == "1")
            .unwrap_or(false),
        tenant: std::env::var("TENANT_ID_OVERRIDE").unwrap_or_else(|_| "common".to_string()),
        branch: std::env::var("DL_US_BRANCH").unwrap_or_else(|_| "published".to_string()),
        fields_hits: Arc::new(AtomicU64::new(0)),
        fields_misses: Arc::new(AtomicU64::new(0)),
        fields_fetches: Arc::new(AtomicU64::new(0)),
        fields_invalidations: Arc::new(AtomicU64::new(0)),
    });
    let monolith = std::env::var("MONOLITH").is_ok();
    let app = Router::new()
        .route("/ui/run", post(ui_run_identity))
        .route("/api/run", post(ui_run_raw_identity))
        .route("/api/run/batch", post(batch_run_identity))
        .route("/api/run/batch/stream", post(batch_run_stream_identity))
        .route("/debug/cache", get(debug_cache))
        .route("/debug/stages", get(debug_stages))
        .route("/debug/alloc", get(debug_alloc))
        .route("/debug/alloc/reset", post(debug_alloc_reset))
        // axum's `Json` extractor defaults to a 2 MB body limit; a dashboard `/api/run/batch`
        // (up to 64 chart bodies, enforced in `batch_run`) can legitimately exceed that, and
        // the data-api router that owns these bodies already accepts 50 MB. Without this the
        // engine edge would reject a batch the backend would have served (413). Single
        // `/api/run` bodies are unaffected in practice and keep the same ceiling.
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state.clone());
    let app = app.merge(crate::ui_api::router());
    let mut app = app.merge(crate::meta_api::router());
    // v8: in the monolith the public app mounts these routes directly and owns `/ping`
    // (exactly one `/ping` route in the process); a standalone charts-engine container
    // keeps its own probe route.
    if !monolith {
        app = app.route("/ping", get(ping));
    }
    // `DL_COMPRESSION=off` is the max-RPS profile (and the lab image default). Keep the
    // middleware out of the stack entirely in that case: with it installed, every
    // `/api/run` still runs `compress`, clones the Accept-Encoding header, resolves an
    // `Extension<Compression>` out of the request extension map and only then returns the
    // untouched response. Enabled deployments keep the exact previous layering.
    let compression = compression_from_env();
    let app = if compression.enabled {
        app.layer(middleware::from_fn(compress))
            .layer(Extension(compression))
    } else {
        app
    };
    // Outermost: one deadline for the whole request. A streaming batch returns its response
    // head immediately, so this bounds "time to an answer", not the length of the stream;
    // the stream keeps its own per-chart deadline in `pipeline::batch_run_stream`.
    let app = with_run_timeout(app, run_timeout());
    crate::local_http::ENGINE_ROUTER.set(app.clone()).ok();
    if monolith {
        return;
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("bind");
    let listener = listener.tap_io(|stream| {
        let _ = stream.set_nodelay(true);
    });
    tracing::info!("charts engine listening on :{port}");
    axum::serve(listener, app).await.expect("serve");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::ServiceExt;

    #[test]
    fn run_deadline_defaults_and_can_be_disabled() {
        assert_eq!(parse_run_timeout(None), Some(Duration::from_secs(120)));
        assert_eq!(parse_run_timeout(Some("0")), None);
        assert_eq!(parse_run_timeout(Some(" 45 ")), Some(Duration::from_secs(45)));
        // A typo must not silently remove the deadline.
        assert_eq!(
            parse_run_timeout(Some("nonsense")),
            Some(Duration::from_secs(120))
        );
    }

    fn slow_router(delay: Duration) -> Router {
        Router::new().route(
            "/slow",
            get(move || async move {
                tokio::time::sleep(delay).await;
                "late"
            }),
        )
    }

    async fn call(router: Router) -> axum::http::StatusCode {
        router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/slow")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn overdue_request_answers_gateway_timeout() {
        let router = with_run_timeout(slow_router(Duration::from_millis(500)), Some(Duration::from_millis(30)));
        assert_eq!(call(router).await, axum::http::StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn fast_request_passes_through() {
        let router = with_run_timeout(slow_router(Duration::from_millis(1)), Some(Duration::from_secs(1)));
        assert_eq!(call(router).await, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn disabled_deadline_does_not_wrap_the_router() {
        let router = with_run_timeout(slow_router(Duration::from_millis(20)), None);
        assert_eq!(call(router).await, axum::http::StatusCode::OK);
    }
}
