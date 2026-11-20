//! HTTP-stack throughput with a dry-run executor.
//!
//! This includes axum routing, extractors, JSON decode, service work and JSON response encode;
//! only the database and the network socket are absent. `cargo bench -p datalens-server --bench http`.

use std::hint::black_box;
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, Request};
use datalens_server::{dry_run_state, router};
use dl_query::Dialect;
use http_body_util::BodyExt;
use tower::ServiceExt;

const ITERATIONS: usize = 10_000;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let app = router(dry_run_state(Dialect::Postgres));
    let body = r#"{"fields":[{"ref":{"type":"id","id":"f_region"},"role_spec":{"role":"row"}},{"ref":{"type":"id","id":"f_sales"},"role_spec":{"role":"measure"}}]}"#;

    runtime.block_on(async {
        for _ in 0..200 {
            let response = request(&app, body).await;
            black_box(response);
        }
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let response = request(&app, body).await;
            black_box(response);
        }
        let elapsed = started.elapsed();
        let ns = elapsed.as_nanos() as f64 / ITERATIONS as f64;
        println!(
            "http result (dry-run): {:.0} ns/op, {:.0} req/s ({} iterations)",
            ns,
            1_000_000_000.0 / ns,
            ITERATIONS
        );
    });
}

async fn request(app: &axum::Router, body: &str) -> usize {
    let request = Request::builder()
        .method("POST")
        .uri("/api/data/v2/datasets/demo/result")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    status as usize + bytes.len()
}
