//! Public single-port entrypoint. Routes are served by the in-process routers; no loopback HTTP.

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::serve::ListenerExt;
use axum::middleware;
use axum::Router;
use std::sync::OnceLock;
use std::time::Duration;
use tower::ServiceExt;

use crate::local_http::{DATA_ROUTER, ENGINE_ROUTER, US_ROUTER};

pub async fn serve(port: u16, _data_api: u16, _us: u16, _engine: u16) -> std::io::Result<()> {
    let mut app = Router::new().route("/ping", any(ping));
    if std::env::var("MONOLITH").is_ok() {
        // v8: the charts-engine is always in-process in the monolith, so mount its routes
        // directly on the public app. `/api/run`, `/ui/run`, `/api/run/batch` and
        // `/debug/*` then skip the fallback prefix dispatcher and the per-request
        // `Router::oneshot` hop — the router only matches routes on the request path it is
        // already on. The engine installs its router during startup (on the single service
        // runtime); wait for it, bounded, because the public listener must come up
        // immediately in a container start.
        let mut engine = None;
        for _ in 0..600 {
            if let Some(router) = ENGINE_ROUTER.get() {
                engine = Some(router.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        match engine {
            Some(engine) => app = app.merge(engine),
            None => tracing::warn!(
                "charts engine router was not installed within 30 s; falling back to prefix dispatch"
            ),
        }
    }
    // The data-api and US routers stay behind the fallback dispatcher on purpose: they are
    // the vendor/express-compatible routers that own their own `/ping` and 404 fallbacks,
    // and in `--dry-run` mock mode the data router is absent (the external mock owns :8081),
    // so the public app must tolerate a missing data router. Those routes are not on the
    // benchmarked chart path; the charts-engine routes above are.
    // Control API is a native Rust router now. Merge it before the fallback prefix
    // dispatcher so `/api/v1/...` is handled here, not forwarded to data-api.
    let app = app.merge(crate::control_api::router());
    // `.private/*` is a service-internal US surface (master token). The monolith serves every
    // router on one public listener, so an external client could otherwise reach it; only
    // loopback callers (the process's own US/data-api/meta-manager clients) may pass.
    let app = app.fallback(any(forward));
    // Apply after `fallback`: `/private/*` is served by the fallback dispatcher, and a layer
    // installed before the fallback does not wrap it.
    let app = app.layer(middleware::from_fn(internal_only));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    // Port chosen by the process, not by a container port map: `PUBLIC_PORT=0` asks the
    // OS for a free port and the bound port is reported (stdout line + `DL_PORTS_FILE`
    // JSON) so the rest of the stack can be wired to it. A non-zero `PUBLIC_PORT` keeps
    // the historical behaviour exactly.
    let bound_port = listener.local_addr().map(|addr| addr.port()).unwrap_or(port);
    println!("PUBLIC_PORT={bound_port}");
    if let Some(path) = std::env::var("DL_PORTS_FILE").ok().filter(|p| !p.is_empty()) {
        let body = serde_json::json!({ "public": bound_port });
        let _ = std::fs::write(&path, body.to_string());
    }
    // The public listener is the one the benchmark traffic is accepted on, and it is the
    // only listener in the process that never set TCP_NODELAY (data_main and engine_main
    // both do). At 30k+ req/s the few-millisecond tail a delayed ACK can add to the
    // head/body writes of a small JSON answer is no longer noise.
    let listener = listener.tap_io(|stream| {
        let _ = stream.set_nodelay(true);
    });
    tracing::info!("v5 public router listening on :{bound_port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
}


/// Loopback plus the private ranges a container network uses (Docker Desktop on macOS
/// hands out `192.168.65.0/24`, compose bridges use `172.16.0.0/12`, Linux hosts may use
/// `10.0.0.0/8`). Anything publicly routable is still refused.
fn is_internal_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Reject the service-internal `/private/*` surface for non-loopback peers.
async fn internal_only(request: Request, next: middleware::Next) -> Response {
    let private = request
        .uri()
        .path()
        .strip_prefix("/private")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    if private {
        // The UI container reaches the monolith over the compose bridge, so its peer is a
        // private address, not loopback. The monolith port is published on the host loopback
        // only, and `/private/*` stays closed to anything outside loopback/RFC1918/link-local.
        let allowed = request
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|ConnectInfo(address)| is_internal_peer(address.ip()))
            .unwrap_or(false);
        if !allowed {
            return (StatusCode::FORBIDDEN, "private API is internal").into_response();
        }
    }
    next.run(request).await
}

async fn ping() -> &'static str { "ok" }

/// `DL_PUBLIC_TIMEOUT_SECS` is deployment-static; cache it instead of reading the
/// environment on every public request.
///
/// `0` disables the timeout wrapper entirely. For the max-RPS profile the handler cannot
/// hang (the benchmark fixtures always answer); skipping the wrapper keeps a timer out of
/// the hot path instead of arming one per request. Any positive value keeps the previous
/// behaviour; missing/invalid still defaults to 120 s.
fn public_timeout() -> Option<Duration> {
    static TIMEOUT: OnceLock<Option<Duration>> = OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        match std::env::var("DL_PUBLIC_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            Some(0) => None,
            Some(seconds) => Some(Duration::from_secs(seconds)),
            None => Some(Duration::from_secs(120)),
        }
    })
}

async fn forward(request: Request) -> Response {
    // Borrow the path for the dispatch decision: no `String` allocation per request.
    let path = request.uri().path();
    let router: Option<Router> = if path.starts_with("/api/run")
        || path.starts_with("/ui/run")
        || path.starts_with("/debug/")
    {
        ENGINE_ROUTER.get().cloned()
    } else if path.starts_with("/api/data") {
        DATA_ROUTER.get().cloned()
    } else if path.starts_with("/v1/")
        || path.starts_with("/v2/")
        || path.starts_with("/private/")
        || path == "/api-docs"
        || path.starts_with("/api-docs/")
        || path == "/metrics"
        || path == "/pool"
        || path == "/ping-db"
        || path == "/ping-db-primary"
    {
        US_ROUTER.get().cloned()
    } else {
        DATA_ROUTER.get().cloned()
    };
    let Some(router) = router else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "router not ready");
    };
    let result = match public_timeout() {
        Some(timeout) => match tokio::time::timeout(timeout, router.oneshot(request)).await {
            Ok(result) => result,
            Err(_) => return text(StatusCode::GATEWAY_TIMEOUT, "public router timeout"),
        },
        None => router.oneshot(request).await,
    };
    match result {
        Ok(response) => response,
        Err(error) => text(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

fn text(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

#[cfg(test)]
mod tests {
    use super::is_internal_peer;

    #[test]
    fn private_api_peers_are_loopback_and_container_networks() {
        for allowed in [
            "127.0.0.1",
            "::1",
            "192.168.65.1", // Docker Desktop host gateway
            "172.17.0.2",   // compose bridge
            "10.1.2.3",
            "169.254.1.1",
            "fd00::1",
        ] {
            assert!(
                is_internal_peer(allowed.parse().expect("ip")),
                "{allowed} must reach /private/*"
            );
        }
        for refused in ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"] {
            assert!(
                !is_internal_peer(refused.parse().expect("ip")),
                "{refused} must not reach /private/*"
            );
        }
    }
}
