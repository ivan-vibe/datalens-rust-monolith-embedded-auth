//! Minimal in-process HTTP client for engine -> data-api.
//!
//! The engine has many reqwest call sites; this type keeps the same get/post/send surface but
//! dispatches data-api URLs to the in-process axum router via tower::Service::oneshot. US/UI
//! URLs still use reqwest.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use std::sync::OnceLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

pub static DATA_ROUTER: OnceLock<axum::Router> = OnceLock::new();
pub static US_ROUTER: OnceLock<axum::Router> = OnceLock::new();
pub static ENGINE_ROUTER: OnceLock<axum::Router> = OnceLock::new();

#[derive(Clone)]
pub struct Client {
    fallback: reqwest::Client,
}

#[derive(Clone)]
pub struct RequestBuilder {
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Option<Bytes>,
    fallback: reqwest::Client,
}

pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Debug)]
pub struct Error(pub String);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// v8.0b: fast loopback HTTP/1.1 keep-alive transport for the mock-track data-api hop
// ---------------------------------------------------------------------------------------------
//
// On the `mock` track `data_direct` has no typed state (the data-api is an external mock
// container), so every chart data call is a small POST/GET to `127.0.0.1:<mock>` that falls
// back to reqwest. For that same-host hop reqwest/hyper pays URL parsing, header-map
// bookkeeping, connection-pool locking and body copies. This is a minimal HTTP/1.1 client
// used only for `http://127.0.0.1|localhost` targets and only when `DL_LOOPBACK_HTTP=1`
// (default off, so the product default path is byte-for-byte unchanged until the mock ladder
// proves it). Any unsupported shape falls back to reqwest **unless the request may already
// have been sent** (then it fails instead of being retried, to avoid duplicate requests).
//
// The caller (the data-api hop) always sends `Accept-Encoding: identity` and a JSON body and
// receives a JSON/msgpack body with `Content-Length` (axum `Json`/the mock both set it).
// Keep-alive connections are cached per authority (bounded); `Connection: close` responses,
// HTTP/1.0 responses and connections that error while idle are dropped; a stale cached
// connection is retried once on a fresh connection (that request was not sent).

#[derive(Debug)]
enum LoopbackError {
    /// Not attempted / definitely not sent: falling back to reqwest is safe.
    Unavailable,
    /// The request may already have been sent; do not retry it.
    Failed(String),
}

fn loopback_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("DL_LOOPBACK_HTTP")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

type LoopbackPool = std::collections::HashMap<String, Vec<tokio::net::TcpStream>>;

static LOOPBACK_POOL: std::sync::OnceLock<tokio::sync::Mutex<LoopbackPool>> =
    std::sync::OnceLock::new();

fn loopback_pool() -> &'static tokio::sync::Mutex<LoopbackPool> {
    LOOPBACK_POOL.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `Some((authority, request-target))` only for a plain-HTTP loopback URL.
fn loopback_target(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, target) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let host = authority.split(':').next().unwrap_or(authority);
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    let target = target.split('#').next().unwrap_or(target);
    Some((authority.to_string(), target.to_string()))
}

fn loopback_request_bytes(
    method: &Method,
    target: &str,
    authority: &str,
    headers: &HeaderMap,
    body: Option<&Bytes>,
) -> Vec<u8> {
    let mut request = Vec::with_capacity(256 + body.map(Bytes::len).unwrap_or(0));
    request.extend_from_slice(method.as_str().as_bytes());
    request.extend_from_slice(b" ");
    request.extend_from_slice(target.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(authority.as_bytes());
    request.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        let name = name.as_str();
        if matches!(name, "host" | "content-length" | "connection" | "transfer-encoding") {
            continue;
        }
        let Ok(value) = value.to_str() else { continue; };
        request.extend_from_slice(name.as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"Connection: keep-alive\r\n");
    if let Some(body) = body {
        request.extend_from_slice(b"Content-Length: ");
        request.extend_from_slice(body.len().to_string().as_bytes());
        request.extend_from_slice(b"\r\n\r\n");
        request.extend_from_slice(body);
    } else {
        request.extend_from_slice(b"\r\n");
    }
    request
}

async fn loopback_read_more(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut Vec<u8>,
) -> Result<(), LoopbackError> {
    let mut chunk = [0u8; 8192];
    let read = stream
        .read(&mut chunk)
        .await
        .map_err(|error| LoopbackError::Failed(format!("loopback read: {error}")))?;
    if read == 0 {
        return Err(LoopbackError::Failed(
            "loopback connection closed mid-response".to_string(),
        ));
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

async fn loopback_read_chunked(
    stream: &mut tokio::net::TcpStream,
    mut buffer: Vec<u8>,
) -> Result<Vec<u8>, LoopbackError> {
    let mut body = Vec::new();
    loop {
        let line_end = loop {
            if let Some(position) = buffer.windows(2).position(|window| window == b"\r\n") {
                break position;
            }
            loopback_read_more(stream, &mut buffer).await?;
        };
        let size_text = std::str::from_utf8(&buffer[..line_end])
            .map_err(|_| LoopbackError::Failed("loopback chunk size is not ASCII".to_string()))?;
        let size = usize::from_str_radix(
            size_text.split(';').next().unwrap_or("").trim(),
            16,
        )
        .map_err(|_| LoopbackError::Failed("loopback bad chunk size".to_string()))?;
        buffer.drain(..line_end + 2);
        if size == 0 {
            loop {
                let trailer_end = loop {
                    if let Some(position) = buffer.windows(2).position(|window| window == b"\r\n")
                    {
                        break position;
                    }
                    loopback_read_more(stream, &mut buffer).await?;
                };
                buffer.drain(..trailer_end + 2);
                if trailer_end == 0 {
                    return Ok(body);
                }
            }
        }
        while buffer.len() < size + 2 {
            loopback_read_more(stream, &mut buffer).await?;
        }
        body.extend_from_slice(&buffer[..size]);
        buffer.drain(..size + 2);
    }
}

type LoopbackResponse = (StatusCode, HeaderMap, Bytes, bool);

async fn loopback_read_response(
    stream: &mut tokio::net::TcpStream,
) -> Result<LoopbackResponse, LoopbackError> {
    let mut buffer = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return Err(LoopbackError::Failed(
                "loopback response head too large".to_string(),
            ));
        }
        loopback_read_more(stream, &mut buffer).await?;
    };
    let head = std::str::from_utf8(&buffer[..head_end])
        .map_err(|_| LoopbackError::Failed("loopback response head is not ASCII".to_string()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| LoopbackError::Failed("loopback response has no status line".to_string()))?;
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| LoopbackError::Failed("loopback bad status line".to_string()))?;
    let status = StatusCode::from_u16(status_code)
        .map_err(|_| LoopbackError::Failed("loopback bad status code".to_string()))?;
    let mut headers = HeaderMap::new();
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut keep_alive = status_line.starts_with("HTTP/1.1");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| LoopbackError::Failed("loopback bad response header".to_string()))?;
        let name = name.trim();
        let value = value.trim();
        let header_name = axum::http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| LoopbackError::Failed("loopback bad response header name".to_string()))?;
        let header_value = axum::http::HeaderValue::from_str(value)
            .map_err(|_| LoopbackError::Failed("loopback bad response header value".to_string()))?;
        if header_name.as_str() == "content-length" {
            content_length = value.parse().ok();
        } else if header_name.as_str() == "transfer-encoding"
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        } else if header_name.as_str() == "connection" && value.eq_ignore_ascii_case("close") {
            keep_alive = false;
        }
        headers.append(header_name, header_value);
    }
    let mut body = buffer.split_off(head_end + 4);
    if chunked {
        body = loopback_read_chunked(stream, body).await?;
    } else if let Some(length) = content_length {
        if length > 64 * 1024 * 1024 {
            return Err(LoopbackError::Failed(
                "loopback response too large".to_string(),
            ));
        }
        while body.len() < length {
            loopback_read_more(stream, &mut body).await?;
        }
        body.truncate(length);
    } else {
        // Unframed body: read to EOF and never reuse the connection.
        keep_alive = false;
        loop {
            let mut chunk = [0u8; 8192];
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|error| LoopbackError::Failed(format!("loopback read: {error}")))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
    }
    Ok((status, headers, Bytes::from(body), keep_alive))
}

async fn loopback_stream(
    authority: &str,
) -> Result<(tokio::net::TcpStream, bool), LoopbackError> {
    let cached = {
        let mut pool = loopback_pool().lock().await;
        pool.get_mut(authority).and_then(|idle| idle.pop())
    };
    if let Some(stream) = cached {
        return Ok((stream, true));
    }
    let connect = tokio::time::timeout(
        internal_timeout(),
        tokio::net::TcpStream::connect(authority),
    )
    .await;
    let stream = match connect {
        Ok(Ok(stream)) => stream,
        _ => return Err(LoopbackError::Unavailable),
    };
    let _ = stream.set_nodelay(true);
    Ok((stream, false))
}

async fn loopback_attempt(
    authority: &str,
    request: &[u8],
) -> Result<LoopbackResponse, LoopbackError> {
    for attempt in 0..2 {
        let (mut stream, reused) = loopback_stream(authority).await?;
        if let Err(error) = stream.write_all(request).await {
            if reused && attempt == 0 {
                continue; // stale idle connection: the request was not sent
            }
            return Err(LoopbackError::Failed(format!("loopback write: {error}")));
        }
        match loopback_read_response(&mut stream).await {
            Ok((status, headers, body, keep_alive)) => {
                if keep_alive {
                    let mut pool = loopback_pool().lock().await;
                    let idle = pool.entry(authority.to_string()).or_default();
                    if idle.len() < 32 {
                        idle.push(stream);
                    }
                }
                return Ok((status, headers, body, keep_alive));
            }
            Err(error) => {
                if reused && attempt == 0 {
                    continue; // stale connection produced no response; retry once fresh
                }
                return Err(error);
            }
        }
    }
    Err(LoopbackError::Failed(
        "loopback data-api request failed".to_string(),
    ))
}

async fn loopback_exchange(
    method: &Method,
    url: &str,
    headers: &HeaderMap,
    body: Option<&Bytes>,
) -> Result<(StatusCode, HeaderMap, Bytes), LoopbackError> {
    if !loopback_enabled() {
        return Err(LoopbackError::Unavailable);
    }
    let Some((authority, target)) = loopback_target(url) else {
        return Err(LoopbackError::Unavailable);
    };
    let request = loopback_request_bytes(method, &target, &authority, headers, body);
    match tokio::time::timeout(internal_timeout(), loopback_attempt(&authority, &request)).await {
        Ok(Ok((status, response_headers, response_body, _keep_alive))) => {
            Ok((status, response_headers, response_body))
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(LoopbackError::Failed(
            "loopback data-api request timed out".to_string(),
        )),
    }
}

impl Client {
    pub fn new() -> Self {
        // The fallback client only serves non-in-process URLs (US/UI/node), which the
        // router-level deadlines do not cover. Without these two bounds a stalled host held
        // the request forever.
        Self {
            fallback: reqwest::Client::builder()
                .tcp_nodelay(true)
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("http client"),
        }
    }
    pub fn get(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder { method: Method::GET, url: url.into(), headers: HeaderMap::new(), body: None, fallback: self.fallback.clone() }
    }
    pub fn post(&self, url: impl Into<String>) -> RequestBuilder {
        RequestBuilder { method: Method::POST, url: url.into(), headers: HeaderMap::new(), body: None, fallback: self.fallback.clone() }
    }
}

/// `DL_INTERNAL_TIMEOUT_SECS` is deployment-static; cache it instead of reading the
/// environment on every in-process data-api call.
pub(crate) fn internal_timeout() -> std::time::Duration {
    static TIMEOUT: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        std::time::Duration::from_secs(
            std::env::var("DL_INTERNAL_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        )
    })
}

/// Request-target (`/path?query`) of an absolute `http(s)://host[:port]/...` URL.
///
/// The engine builds these URLs from a fixed local base plus a dataset id, so splitting the
/// string is exact and avoids a per-call `reqwest::Url` allocation + normalisation on the
/// in-process data-api path. `None` sends the caller through the full parser, which also
/// validates anything unusual.
fn request_target(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let path = match rest.find('/') {
        Some(index) => &rest[index..],
        None => "/",
    };
    if path.is_empty() {
        return Some("/".to_string());
    }
    Some(path.to_string())
}

impl RequestBuilder {
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        for (name, value) in headers {
            if let Some(name) = name {
                self.headers.insert(name, value);
            }
        }
        self
    }
    /// Add a compile-time-known header without parsing the name/value on every request.
    pub fn header_static(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
        self
    }
    pub fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        if let (Ok(name), Ok(value)) = (
            name.as_ref().parse::<axum::http::HeaderName>(),
            value.as_ref().parse::<axum::http::HeaderValue>(),
        ) {
            self.headers.insert(name, value);
        }
        self
    }
    pub fn json<T: serde::Serialize>(mut self, value: &T) -> Self {
        self.body = serde_json::to_vec(value).ok().map(Bytes::from);
        self.header("content-type", "application/json")
    }
    pub fn body(mut self, body: impl Into<Bytes>) -> Self { self.body = Some(body.into()); self }
    pub async fn send(self) -> Result<Response, Error> {
        let path = match request_target(&self.url) {
            Some(path) => path,
            None => {
                let parsed = reqwest::Url::parse(&self.url).map_err(|e| Error(e.to_string()))?;
                if let Some(query) = parsed.query() {
                    format!("{}?{}", parsed.path(), query)
                } else {
                    parsed.path().to_string()
                }
            }
        };
        if path.starts_with("/private/") || path.starts_with("/v1/") || path.starts_with("/v2/") {
            let headers = self.headers.iter()
                .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.to_string(), value.to_string())))
                .collect::<Vec<_>>();
            let response = crate::local_us::call(path, headers).await.ok_or_else(|| Error("US transport not installed".to_string()))?;
            return Ok(Response {
                status: StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                headers: HeaderMap::new(),
                body: Bytes::from(response.body),
            });
        }
        let router = if path.starts_with("/api/data") {
            DATA_ROUTER.get().cloned()
        } else {
            None
        };
        let Some(router) = router else {
            // v8.0b: same-host data-api hop (mock track / loopback fallback). Env-gated and
            // only for plain-HTTP loopback URLs; otherwise this is a cheap `Unavailable`.
            match loopback_exchange(&self.method, &self.url, &self.headers, self.body.as_ref())
                .await
            {
                Ok((status, headers, body)) => return Ok(Response { status, headers, body }),
                Err(LoopbackError::Unavailable) => {}
                Err(LoopbackError::Failed(message)) => return Err(Error(message)),
            }
            let parsed = reqwest::Url::parse(&self.url).map_err(|e| Error(e.to_string()))?;
            let mut request = self.fallback.request(self.method.clone(), parsed);
            // The builder is consumed here, so move the headers instead of cloning the map.
            request = request.headers(self.headers);
            if let Some(body) = self.body { request = request.body(body); }
            let response = request.send().await.map_err(|e| Error(e.to_string()))?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.bytes().await.map_err(|e| Error(e.to_string()))?;
            return Ok(Response { status, headers, body });
        };
        let mut builder = Request::builder().method(self.method).uri(path);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let request = builder.body(self.body.map(Body::from).unwrap_or_else(Body::empty))
            .map_err(|e| Error(e.to_string()))?;
        let response = tokio::time::timeout(internal_timeout(), router.oneshot(request))
            .await
            .map_err(|_| Error("in-process router timeout".to_string()))?
            .map_err(|e| Error(e.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 50 * 1024 * 1024)
            .await
            .map_err(|e| Error(e.to_string()))?;
        Ok(Response { status, headers, body })
    }
}

impl Response {
    pub fn status(&self) -> reqwest::StatusCode { reqwest::StatusCode::from_u16(self.status.as_u16()).unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR) }
    pub fn headers(&self) -> &HeaderMap { &self.headers }
    pub async fn bytes(self) -> Result<Bytes, Error> { Ok(self.body) }
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|e| Error(e.to_string()))
    }
    pub fn bytes_stream(self) -> futures::stream::Iter<std::vec::IntoIter<Result<Bytes, Error>>> {
        futures::stream::iter(vec![Ok(self.body)])
    }
}
