//! Prometheus metrics (v2): request counters, latency histogram and in-flight
//! gauge. Hand-rolled with atomics so the hot path stays allocation-free and
//! the service keeps its "no extra runtime dependencies" property.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const VERSION: &str = "2.0.0";
/// DataLens release this port is behaviour-compatible with.
pub const UPSTREAM_VERSION: &str = "1.196.0";

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

pub async fn scrape() -> Response {
    let mut text = String::with_capacity(4096);

    text.push_str("# HELP us_rs_http_requests_total Total HTTP requests handled.\n");
    text.push_str("# TYPE us_rs_http_requests_total counter\n");
    text.push_str(&format!(
        "us_rs_http_requests_total{{{}}} {}\n",
        "status=\"2xx\"",
        STATUS_2XX.load(Ordering::Relaxed)
    ));
    text.push_str(&format!(
        "us_rs_http_requests_total{{status=\"3xx\"}} {}\n",
        STATUS_3XX.load(Ordering::Relaxed)
    ));
    text.push_str(&format!(
        "us_rs_http_requests_total{{status=\"4xx\"}} {}\n",
        STATUS_4XX.load(Ordering::Relaxed)
    ));
    text.push_str(&format!(
        "us_rs_http_requests_total{{status=\"5xx\"}} {}\n",
        STATUS_5XX.load(Ordering::Relaxed)
    ));

    text.push_str("# HELP us_rs_http_request_duration_seconds Request latency.\n");
    text.push_str("# TYPE us_rs_http_request_duration_seconds histogram\n");
    for (index, bucket) in BUCKETS_US.iter().enumerate() {
        text.push_str(&format!(
            "us_rs_http_request_duration_seconds_bucket{{le=\"{}\"}} {}\n",
            *bucket as f64 / 1_000_000.0,
            DURATION_BUCKETS[index].load(Ordering::Relaxed)
        ));
    }
    text.push_str(&format!(
        "us_rs_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {}\n",
        DURATION_COUNT.load(Ordering::Relaxed)
    ));
    text.push_str(&format!(
        "us_rs_http_request_duration_seconds_sum {:.6}\n",
        DURATION_SUM_US.load(Ordering::Relaxed) as f64 / 1_000_000.0
    ));
    text.push_str(&format!(
        "us_rs_http_request_duration_seconds_count {}\n",
        DURATION_COUNT.load(Ordering::Relaxed)
    ));

    text.push_str("# HELP us_rs_http_in_flight Requests currently being served.\n");
    text.push_str("# TYPE us_rs_http_in_flight gauge\n");
    text.push_str(&format!(
        "us_rs_http_in_flight {}\n",
        IN_FLIGHT.load(Ordering::Relaxed)
    ));

    text.push_str("# HELP us_rs_build_info Build information.\n");
    text.push_str("# TYPE us_rs_build_info gauge\n");
    text.push_str(&format!(
        "us_rs_build_info{{version=\"{VERSION}\",upstream=\"{UPSTREAM_VERSION}\"}} 1\n"
    ));

    let mut response = (StatusCode::OK, text).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}
