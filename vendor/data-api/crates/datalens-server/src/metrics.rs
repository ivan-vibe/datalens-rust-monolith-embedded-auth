//! Data-api metrics: `GET /metrics` (Prometheus text) and `GET /unistat/` (JSON pairs).
//!
//! Python serves both from `dl_api_lib.app.data_api.resources`:
//!
//! * `DSDataApiMetricsView` (`dl_core.aio.metrics_view.MetricsView`) dumps
//!   `statcommons.unistat` series with `dump_for_prometheus`; with `UWSGI_STATS` unset the body
//!   is empty, and when it is set the series are uwsgi's own request counters.
//! * `UnistatView` reads the uwsgi stats socket and answers `json_response(list(result.items()))`,
//!   i.e. a JSON array of `[name, value]` pairs, with auth skipped.
//!
//! The Rust service has no uwsgi in front of it, so the equivalent series are the request
//! counters and latency histogram of this process, sampled by the middleware below. The
//! `/unistat/` payload keeps Python's *shape* (pairs), not its uwsgi-specific names.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// DataLens release this port is behaviour-compatible with.
pub const UPSTREAM_VERSION: &str = "0.2457.0";

/// Histogram bounds in microseconds. Same grid as the `us` service, so both scrapes line up.
const BUCKETS_US: [u64; 12] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
];

static REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
static STATUS_2XX: AtomicU64 = AtomicU64::new(0);
static STATUS_3XX: AtomicU64 = AtomicU64::new(0);
static STATUS_4XX: AtomicU64 = AtomicU64::new(0);
static STATUS_5XX: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicI64 = AtomicI64::new(0);
static DURATION_COUNT: AtomicU64 = AtomicU64::new(0);
static DURATION_SUM_US: AtomicU64 = AtomicU64::new(0);
static DURATION_BUCKETS: [AtomicU64; 12] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Record one finished data-api request. Installed as the outermost router layer, so it also
/// covers the 401s of the auth middleware.
pub async fn middleware(request: Request, next: Next) -> Response {
    IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let started = std::time::Instant::now();

    let response = next.run(request).await;

    let micros = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    let status = response.status().as_u16();

    REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    match status {
        200..=299 => STATUS_2XX.fetch_add(1, Ordering::Relaxed),
        300..=399 => STATUS_3XX.fetch_add(1, Ordering::Relaxed),
        400..=499 => STATUS_4XX.fetch_add(1, Ordering::Relaxed),
        _ => STATUS_5XX.fetch_add(1, Ordering::Relaxed),
    };

    DURATION_COUNT.fetch_add(1, Ordering::Relaxed);
    DURATION_SUM_US.fetch_add(micros, Ordering::Relaxed);
    for (index, bucket) in BUCKETS_US.iter().enumerate() {
        if micros <= *bucket {
            DURATION_BUCKETS[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    response
}

/// `GET /metrics`: `dump_for_prometheus`'s text format (`text/plain; version=0.0.4`).
pub async fn scrape() -> Response {
    let mut text = String::with_capacity(4096);

    text.push_str("# HELP dl_dataapi_http_requests_total Total data-api HTTP requests handled.\n");
    text.push_str("# TYPE dl_dataapi_http_requests_total counter\n");
    for (class, counter) in [
        ("2xx", &STATUS_2XX),
        ("3xx", &STATUS_3XX),
        ("4xx", &STATUS_4XX),
        ("5xx", &STATUS_5XX),
    ] {
        text.push_str(&format!(
            "dl_dataapi_http_requests_total{{status=\"{class}\"}} {}\n",
            counter.load(Ordering::Relaxed)
        ));
    }

    text.push_str("# HELP dl_dataapi_http_request_duration_seconds Request latency.\n");
    text.push_str("# TYPE dl_dataapi_http_request_duration_seconds histogram\n");
    for (index, bucket) in BUCKETS_US.iter().enumerate() {
        text.push_str(&format!(
            "dl_dataapi_http_request_duration_seconds_bucket{{le=\"{}\"}} {}\n",
            *bucket as f64 / 1_000_000.0,
            DURATION_BUCKETS[index].load(Ordering::Relaxed)
        ));
    }
    text.push_str(&format!(
        "dl_dataapi_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {}\n",
        DURATION_COUNT.load(Ordering::Relaxed)
    ));
    text.push_str(&format!(
        "dl_dataapi_http_request_duration_seconds_sum {:.6}\n",
        DURATION_SUM_US.load(Ordering::Relaxed) as f64 / 1_000_000.0
    ));
    text.push_str(&format!(
        "dl_dataapi_http_request_duration_seconds_count {}\n",
        DURATION_COUNT.load(Ordering::Relaxed)
    ));

    text.push_str("# HELP dl_dataapi_http_in_flight Requests currently being served.\n");
    text.push_str("# TYPE dl_dataapi_http_in_flight gauge\n");
    text.push_str(&format!(
        "dl_dataapi_http_in_flight {}\n",
        IN_FLIGHT.load(Ordering::Relaxed)
    ));

    text.push_str("# HELP dl_dataapi_build_info Build information.\n");
    text.push_str("# TYPE dl_dataapi_build_info gauge\n");
    text.push_str(&format!(
        "dl_dataapi_build_info{{upstream=\"{UPSTREAM_VERSION}\"}} 1\n"
    ));

    let mut response = (StatusCode::OK, text).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}

/// `GET /unistat/`: `list(result.items())`, i.e. a JSON array of `[name, value]` pairs.
pub async fn unistat() -> axum::Json<Vec<(&'static str, u64)>> {
    axum::Json(vec![
        ("requests_total", REQUESTS_TOTAL.load(Ordering::Relaxed)),
        ("requests_2xx", STATUS_2XX.load(Ordering::Relaxed)),
        ("requests_3xx", STATUS_3XX.load(Ordering::Relaxed)),
        ("requests_4xx", STATUS_4XX.load(Ordering::Relaxed)),
        ("requests_5xx", STATUS_5XX.load(Ordering::Relaxed)),
        (
            "requests_in_flight",
            IN_FLIGHT.load(Ordering::Relaxed).max(0) as u64,
        ),
        (
            "request_time_us_sum",
            DURATION_SUM_US.load(Ordering::Relaxed),
        ),
        (
            "request_time_us_count",
            DURATION_COUNT.load(Ordering::Relaxed),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/metrics", get(scrape))
            .route("/unistat/", get(unistat))
            .route("/ping", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(middleware))
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_request_moves_the_counters_and_the_scrape_is_prometheus_text() {
        let before = REQUESTS_TOTAL.load(Ordering::Relaxed);
        let response = app()
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // The counters are process-wide and the test harness runs cases in parallel.
        assert!(REQUESTS_TOTAL.load(Ordering::Relaxed) > before);

        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = body_text(response).await;
        assert!(
            body.contains("# TYPE dl_dataapi_http_requests_total counter"),
            "{body}"
        );
        assert!(
            body.contains("dl_dataapi_http_request_duration_seconds_bucket{le=\"+Inf\"}"),
            "{body}"
        );
        assert!(
            body.contains("dl_dataapi_build_info{upstream=\"0.2457.0\"} 1"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn unistat_answers_a_list_of_name_value_pairs() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/unistat/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        let pairs = body
            .as_array()
            .expect("a JSON array, as Python's list(items())");
        assert!(!pairs.is_empty());
        assert!(pairs
            .iter()
            .all(|pair| pair.as_array().is_some_and(|p| p.len() == 2)));
        assert!(pairs.iter().any(|pair| pair[0] == "requests_total"));
    }
}
