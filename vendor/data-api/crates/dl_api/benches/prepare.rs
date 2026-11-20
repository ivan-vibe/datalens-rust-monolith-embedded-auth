//! A small, dependency-free microbenchmark for the request-preparation path.
//!
//! Run with `cargo bench -p dl_api --bench prepare` (release profile). It measures the part
//! that runs before SQL execution: dataset updates, legend, field types and compilation.

use std::hint::black_box;
use std::time::Instant;

use dl_api::executor::StaticExecutor;
use dl_api::testing::demo_dataset;
use dl_api::{FieldsItem, ResultRequest, Service};
use dl_core::FieldRef;
use dl_query::{Dialect, Role};

fn request() -> ResultRequest {
    ResultRequest {
        fields: vec![
            FieldsItem::new(FieldRef::id("f_region"), Role::Row),
            FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
        ],
        ..ResultRequest::default()
    }
}

fn main() {
    let service = Service::new(StaticExecutor::new(Vec::new()), Dialect::Postgres);
    let dataset = demo_dataset();
    let request = request();

    // Warm up the caches and the branch predictor before measuring.
    for _ in 0..200 {
        black_box(service.prepare_result(&dataset, &request).unwrap());
    }

    let iterations = 5_000;
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(service.prepare_result(&dataset, &request).unwrap());
    }
    let elapsed = started.elapsed();
    println!(
        "prepare_result: {:.0} ns/request ({} iterations, {:?})",
        elapsed.as_nanos() as f64 / iterations as f64,
        iterations,
        elapsed
    );

    let (types, data_types) = dl_query::field_types_and_data_types(&dataset, Dialect::Postgres);
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(
            dl_api::LegendFormalizer::for_result(&dataset, &request)
                .with_field_types(types.clone())
                .with_data_types(data_types.clone())
                .make()
                .unwrap(),
        );
    }
    println!(
        "legend only: {:.0} ns/request",
        started.elapsed().as_nanos() as f64 / iterations as f64
    );

    let started = Instant::now();
    for _ in 0..iterations {
        black_box(dl_query::field_types_and_data_types(
            &dataset,
            Dialect::Postgres,
        ));
    }
    println!(
        "field types + data types (one pass): {:.0} ns/request",
        started.elapsed().as_nanos() as f64 / iterations as f64
    );

    // Cost of the deep clone that the old no-op update path paid on every chart.
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(dl_api::updates::apply_updates(&dataset, &[]).unwrap());
    }
    println!(
        "deep dataset clone (old no-op path): {:.0} ns/request",
        started.elapsed().as_nanos() as f64 / iterations as f64
    );

    // The edit path still owns a private copy; comparing the two makes the no-op case visible.
    let mut edited = request.clone();
    edited.updates = vec![dl_api::UpdateAction::Other];
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(service.prepare_result(&dataset, &edited).is_err());
    }
    println!(
        "prepare_result with an invalid update: {:.0} ns/request",
        started.elapsed().as_nanos() as f64 / iterations as f64
    );
}
