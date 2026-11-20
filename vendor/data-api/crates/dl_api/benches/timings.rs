//! Stage timings for the demo `result` request, without a database.
//!
//! Five batches are collected and summarised by p50/p95 so a single noisy run cannot hide the
//! shape of the request. `cargo bench -p dl_api --bench timings`.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use dl_api::executor::StaticExecutor;
use dl_api::testing::demo_dataset;
use dl_api::trace::{with_trace, StageKind, Summary, Trace};
use dl_api::{FieldsItem, ResultRequest, Service};
use dl_core::FieldRef;
use dl_query::{Dialect, Role};

const BATCHES: usize = 5;
const PER_BATCH: usize = 1_000;

fn request() -> ResultRequest {
    ResultRequest {
        fields: vec![
            FieldsItem::new(FieldRef::id("f_region"), Role::Row),
            FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
        ],
        ..ResultRequest::default()
    }
}

fn us(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn pct(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut values = values.to_vec();
    values.sort_by(|left, right| left.partial_cmp(right).unwrap());
    let index = ((values.len() - 1) as f64 * p).round() as usize;
    values[index]
}

#[derive(Default)]
struct Aggregate {
    total: Vec<f64>,
    work: Vec<f64>,
    wait: Vec<f64>,
    stages: BTreeMap<(&'static str, StageKind), Vec<f64>>,
}

impl Aggregate {
    fn push(&mut self, summary: &Summary) {
        self.total.push(us(summary.total));
        self.work.push(us(summary.work));
        self.wait.push(us(summary.wait));
        for stage in &summary.stages {
            self.stages
                .entry((stage.name, stage.kind))
                .or_default()
                .push(us(stage.total));
        }
    }
}

fn row(name: &str, values: &[f64]) {
    println!(
        "{name:<24} mean {:>9.1} us  p50 {:>9.1}  p95 {:>9.1}",
        values.iter().sum::<f64>() / values.len() as f64,
        pct(values, 0.50),
        pct(values, 0.95)
    );
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let service = Service::new(StaticExecutor::new(Vec::new()), Dialect::Postgres);
    let dataset = demo_dataset();
    let request = request();

    runtime.block_on(async {
        for _ in 0..200 {
            let _ = service.result(&dataset, &request).await.unwrap();
        }
        let mut aggregate = Aggregate::default();
        for _ in 0..BATCHES {
            for _ in 0..PER_BATCH {
                let trace = Arc::new(Trace::new());
                let response = with_trace(Arc::clone(&trace), service.result(&dataset, &request))
                    .await
                    .unwrap();
                black_box(response);
                aggregate.push(&trace.summary());
            }
        }

        println!("samples: {}", aggregate.total.len());
        row("total", &aggregate.total);
        row("work", &aggregate.work);
        row("wait", &aggregate.wait);
        println!();
        for ((name, kind), values) in &aggregate.stages {
            row(&format!("{name} [{}]", kind_name(*kind)), values);
        }
    });
}

fn kind_name(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Work => "work",
        StageKind::Wait => "wait",
        StageKind::Detail => "detail",
    }
}
