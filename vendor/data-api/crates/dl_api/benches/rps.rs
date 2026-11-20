//! Demo-request throughput without a database.
//!
//! The executor is a `StaticExecutor`: no socket, no Postgres/ClickHouse, no network. What is
//! measured is exactly the service work — request decode (already done by the caller here),
//! dataset updates, legend, field metadata, SQL compilation, row decoding and response build.
//!
//! `cargo bench -p dl_api --bench rps`

use std::hint::black_box;
use std::time::Instant;

use dl_api::executor::StaticExecutor;
use dl_api::testing::demo_dataset;
use dl_api::{
    DistinctRequest, FieldsItem, PivotRequest, PreviewRequest, RangeRequest, ResultRequest, Service,
};
use dl_core::FieldRef;
use dl_query::{Dialect, Role};

const ITERATIONS: usize = 20_000;

fn result_request() -> ResultRequest {
    ResultRequest {
        fields: vec![
            FieldsItem::new(FieldRef::id("f_region"), Role::Row),
            FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
        ],
        ..ResultRequest::default()
    }
}

fn report(name: &str, elapsed: std::time::Duration, iterations: usize) {
    let ns = elapsed.as_nanos() as f64 / iterations as f64;
    println!(
        "{name:<24} {:>10.0} ns/op {:>12.0} req/s",
        ns,
        1_000_000_000.0 / ns
    );
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let service = Service::new(
        StaticExecutor::new(vec![
            vec![Some("Europe".into()), Some("42".into())],
            vec![Some("Asia".into()), Some("7".into())],
        ]),
        Dialect::Postgres,
    );
    let dataset = demo_dataset();

    runtime.block_on(async {
        macro_rules! bench {
            ($name:expr, $iterations:expr, $body:expr) => {{
                for _ in 0..200 {
                    black_box($body.await.unwrap());
                }
                let started = Instant::now();
                for _ in 0..$iterations {
                    black_box($body.await.unwrap());
                }
                report($name, started.elapsed(), $iterations);
            }};
        }

        let result = result_request();
        bench!(
            "result (2 fields)",
            ITERATIONS,
            service.result(&dataset, &result)
        );

        let pivot = PivotRequest {
            fields: result.fields.clone(),
            ..PivotRequest::default()
        };
        bench!(
            "pivot (2 fields)",
            ITERATIONS,
            service.pivot(&dataset, &pivot)
        );

        let preview = PreviewRequest {
            dataset: Some(demo_dataset()),
            ..PreviewRequest::default()
        };
        bench!("preview (demo)", ITERATIONS, service.preview(&preview));

        let distinct = DistinctRequest {
            fields: vec![FieldsItem::new(FieldRef::id("f_region"), Role::Row)],
            ..DistinctRequest::default()
        };
        bench!(
            "values/distinct",
            ITERATIONS,
            service.distinct(&dataset, &distinct)
        );

        let range = RangeRequest {
            fields: vec![FieldsItem::new(FieldRef::id("f_sales"), Role::Row)],
            ..RangeRequest::default()
        };
        bench!("values/range", ITERATIONS, service.range(&dataset, &range));
    });
}
