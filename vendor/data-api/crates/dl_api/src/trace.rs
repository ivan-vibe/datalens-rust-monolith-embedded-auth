//! Per-request stage timing.
//!
//! The trace is a task-local created by the HTTP middleware and recorded by the service. It is
//! strictly request-scoped: stages are never stored on `Service`, so one request cannot observe
//! another request's timings. `Work` is CPU time spent in the service (prepare, decode, pivot,
//! response assembly); `Wait` is time the request spent waiting (database, `us`, DSN resolution).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StageKind {
    /// CPU work in this process.
    Work,
    /// Waiting for an external resource (database, network, `us`).
    Wait,
    /// A sub-stage inside a larger stage; shown in the log but not added to work/wait totals.
    Detail,
}

impl StageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Wait => "wait",
            Self::Detail => "detail",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Stage {
    pub kind: StageKind,
    pub name: &'static str,
    pub duration: Duration,
}

/// A request-wide trace. Share it with the service through [`with_trace`].
pub struct Trace {
    started: Instant,
    stages: Mutex<Vec<Stage>>,
}

impl Trace {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            stages: Mutex::new(Vec::new()),
        }
    }

    pub fn record(&self, kind: StageKind, name: &'static str, duration: Duration) {
        if let Ok(mut stages) = self.stages.lock() {
            stages.push(Stage {
                kind,
                name,
                duration,
            });
        }
    }

    /// Aggregate stages by `(kind, name)` and add the wall-clock total.
    pub fn summary(&self) -> Summary {
        let mut grouped: BTreeMap<(&'static str, &'static str), StageStat> = BTreeMap::new();
        if let Ok(stages) = self.stages.lock() {
            for stage in stages.iter() {
                let key = (stage.name, stage.kind.as_str());
                let entry = grouped.entry(key).or_insert_with(|| StageStat {
                    name: stage.name,
                    kind: stage.kind,
                    count: 0,
                    total: Duration::ZERO,
                    max: Duration::ZERO,
                });
                entry.count += 1;
                entry.total += stage.duration;
                entry.max = entry.max.max(stage.duration);
            }
        }
        let stages: Vec<StageStat> = grouped.into_values().collect();
        let work = stages
            .iter()
            .filter(|stage| stage.kind == StageKind::Work)
            .map(|stage| stage.total)
            .sum();
        let wait = stages
            .iter()
            .filter(|stage| stage.kind == StageKind::Wait)
            .map(|stage| stage.total)
            .sum();
        Summary {
            total: self.started.elapsed(),
            work,
            wait,
            stages,
        }
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct StageStat {
    pub name: &'static str,
    pub kind: StageKind,
    pub count: u32,
    pub total: Duration,
    pub max: Duration,
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub total: Duration,
    pub work: Duration,
    pub wait: Duration,
    pub stages: Vec<StageStat>,
}

impl Summary {
    /// One JSON array, suitable for a `tracing` field.
    pub fn stages_json(&self) -> String {
        let stages: Vec<_> = self
            .stages
            .iter()
            .map(|stage| {
                json!({
                    "name": stage.name,
                    "kind": stage.kind.as_str(),
                    "count": stage.count,
                    "total_us": stage.total.as_secs_f64() * 1_000_000.0,
                    "max_us": stage.max.as_secs_f64() * 1_000_000.0,
                })
            })
            .collect();
        serde_json::Value::Array(stages).to_string()
    }
}

tokio::task_local! {
    static TRACE: Arc<Trace>;
}

/// Install `trace` for the duration of `future`.
pub async fn with_trace<F: Future>(trace: Arc<Trace>, future: F) -> F::Output {
    TRACE.scope(trace, future).await
}

pub fn current() -> Option<Arc<Trace>> {
    TRACE.try_with(Arc::clone).ok()
}

pub fn record_work(name: &'static str, duration: Duration) {
    if let Some(trace) = current() {
        trace.record(StageKind::Work, name, duration);
    }
}

pub fn record_wait(name: &'static str, duration: Duration) {
    if let Some(trace) = current() {
        trace.record(StageKind::Wait, name, duration);
    }
}

pub fn record_detail(name: &'static str, duration: Duration) {
    if let Some(trace) = current() {
        trace.record(StageKind::Detail, name, duration);
    }
}

/// Time a synchronous block as work.
pub fn work<T>(name: &'static str, body: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = body();
    record_work(name, started.elapsed());
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stages_are_scoped_and_aggregated() {
        assert!(current().is_none());
        let trace = Arc::new(Trace::new());
        let summary = with_trace(Arc::clone(&trace), async {
            record_work("prepare", Duration::from_micros(10));
            record_work("prepare", Duration::from_micros(20));
            record_wait("db", Duration::from_millis(5));
            trace.summary()
        })
        .await;
        assert!(current().is_none());
        assert_eq!(summary.work, Duration::from_micros(30));
        assert_eq!(summary.wait, Duration::from_millis(5));
        assert_eq!(summary.stages.len(), 2);
        assert!(summary.stages_json().contains("prepare"));
    }
}
