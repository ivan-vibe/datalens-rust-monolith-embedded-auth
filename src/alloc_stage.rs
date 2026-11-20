//! Per-stage allocation counters for the chart request path (feature `alloc-profile`).
//!
//! [`crate::alloc_profile`] counts allocations process-wide. This module attributes the
//! *deltas* between explicit stage marks to named stages, so a profile build can answer
//!
//! ```text
//! GET /debug/alloc        -> totals + {"enabled": true}
//! GET /debug/alloc/stages -> per-stage allocs/bytes/calls + totals
//! POST /debug/alloc/reset -> zero both
//! ```
//!
//! Stages are the points the chart path already times in `pipeline::record_stage` plus the
//! two serialization phases:
//!
//! * `request.start`      – request setup before the first explicit mark;
//! * `graph.fields`       – dataset schema resolution of a graph chart;
//! * `graph.fetch`        – data-api fetch of a graph chart;
//! * `graph.finish`       – Rust graph builder (`graph_engine`/`gravity_*`/`d3_area`);
//! * `envelope.serialize` – envelope construction + `serde_json::to_writer` of one chart;
//! * `batch.serialize`    – `POST /api/run/batch` response assembly/serialization.
//!
//! Usage protocol (used by the measurement in `patches/opt1_alloc.md`):
//!
//! 1. `POST /debug/alloc/reset`;
//! 2. replay the 21 captured `POST /api/run` bodies **sequentially** (one request in
//!    flight), then `GET /debug/alloc/stages`;
//! 3. for `batch.serialize`, replay the same 21 bodies as one `POST /api/run/batch`.
//!
//! The counters are global deltas, so attribution is only meaningful while one request is
//! in flight — that is exactly how an allocation profile is normally captured. Under
//! concurrency the per-stage numbers mix requests (totals stay correct).
//!
//! `begin_request`/`mark` are called by the chart path only in profile builds. In the
//! default build the calls are `#[inline(always)]` empty functions, so the production image
//! pays nothing and `/debug/alloc{,/stages}` report `"enabled": false`.

#[cfg(feature = "alloc-profile")]
mod imp {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use serde_json::json;

    use crate::alloc_profile;

    /// Stage order is part of the JSON contract (it is always emitted in this order).
    pub const STAGE_NAMES: [&str; 6] = [
        "request.start",
        "graph.fields",
        "graph.fetch",
        "graph.finish",
        "envelope.serialize",
        "batch.serialize",
    ];

    /// One stage row: accumulated deltas + the last observed delta/call.
    struct Slot {
        calls: AtomicU64,
        allocs: AtomicU64,
        alloc_bytes: AtomicU64,
        reallocs: AtomicU64,
        dealloc_bytes: AtomicU64,
        last_allocs: AtomicU64,
        last_alloc_bytes: AtomicU64,
    }

    impl Slot {
        const fn new() -> Self {
            Self {
                calls: AtomicU64::new(0),
                allocs: AtomicU64::new(0),
                alloc_bytes: AtomicU64::new(0),
                reallocs: AtomicU64::new(0),
                dealloc_bytes: AtomicU64::new(0),
                last_allocs: AtomicU64::new(0),
                last_alloc_bytes: AtomicU64::new(0),
            }
        }

        fn clear(&self) {
            for counter in [
                &self.calls,
                &self.allocs,
                &self.alloc_bytes,
                &self.reallocs,
                &self.dealloc_bytes,
                &self.last_allocs,
                &self.last_alloc_bytes,
            ] {
                counter.store(0, Ordering::Relaxed);
            }
        }
    }

    static SLOTS: [Slot; STAGE_NAMES.len()] = [
        Slot::new(),
        Slot::new(),
        Slot::new(),
        Slot::new(),
        Slot::new(),
        Slot::new(),
    ];

    #[derive(Clone, Copy, Default)]
    struct Snapshot {
        allocs: u64,
        alloc_bytes: u64,
        reallocs: u64,
        dealloc_bytes: u64,
    }

    fn totals() -> Snapshot {
        Snapshot {
            allocs: alloc_profile::ALLOCS.load(Ordering::Relaxed),
            alloc_bytes: alloc_profile::ALLOC_BYTES.load(Ordering::Relaxed),
            reallocs: alloc_profile::REALLOCS.load(Ordering::Relaxed),
            dealloc_bytes: alloc_profile::DEALLOC_BYTES.load(Ordering::Relaxed),
        }
    }

    struct State {
        last: Snapshot,
        current: usize,
        active: bool,
        requests: u64,
    }

    fn state() -> &'static Mutex<State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE.get_or_init(|| {
            Mutex::new(State {
                last: totals(),
                current: 0,
                active: false,
                requests: 0,
            })
        })
    }

    fn lock() -> std::sync::MutexGuard<'static, State> {
        // A poisoned lock only means another thread panicked while holding it; the counters
        // are still consistent enough for a profiler, so recover instead of panicking.
        state().lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attribute the allocations since `last` to `current`, then advance `last`.
    fn close_pending(state: &mut State) {
        if !state.active {
            return;
        }
        let now = totals();
        let slot = &SLOTS[state.current];
        let allocs = now.allocs.saturating_sub(state.last.allocs);
        let alloc_bytes = now.alloc_bytes.saturating_sub(state.last.alloc_bytes);
        let reallocs = now.reallocs.saturating_sub(state.last.reallocs);
        let dealloc_bytes = now.dealloc_bytes.saturating_sub(state.last.dealloc_bytes);
        slot.calls.fetch_add(1, Ordering::Relaxed);
        slot.allocs.fetch_add(allocs, Ordering::Relaxed);
        slot.alloc_bytes.fetch_add(alloc_bytes, Ordering::Relaxed);
        slot.reallocs.fetch_add(reallocs, Ordering::Relaxed);
        slot.dealloc_bytes
            .fetch_add(dealloc_bytes, Ordering::Relaxed);
        slot.last_allocs.store(allocs, Ordering::Relaxed);
        slot.last_alloc_bytes.store(alloc_bytes, Ordering::Relaxed);
        state.last = now;
    }

    /// Start a request: count it and make the allocations after this point attributable.
    pub fn begin_request() {
        let mut state = lock();
        close_pending(&mut state);
        state.active = true;
        state.requests = state.requests.saturating_add(1);
        state.current = 0; // request.start
        state.last = totals();
    }

    /// Close the current stage and enter `stage` (one of [`STAGE_NAMES`]).
    pub fn mark(stage: &'static str) {
        let Some(index) = STAGE_NAMES.iter().position(|name| *name == stage) else {
            return;
        };
        let mut state = lock();
        close_pending(&mut state);
        if !state.active {
            // A mark without `begin_request` (worker warm-up, tests): start a session so the
            // stage is still attributable instead of silently mixing into the previous one.
            state.active = true;
            state.last = totals();
        }
        state.current = index;
    }

    /// Zero every stage and the request counter. The global allocator counters are reset by
    /// the `/debug/alloc/reset` handler via [`alloc_profile::reset`].
    pub fn reset() {
        for slot in &SLOTS {
            slot.clear();
        }
        let mut state = lock();
        state.requests = 0;
        state.current = 0;
        state.active = false;
        state.last = totals();
    }

    pub fn snapshot_json() -> serde_json::Value {
        let state = lock();
        let mut stages = serde_json::Map::with_capacity(STAGE_NAMES.len());
        for (index, name) in STAGE_NAMES.iter().enumerate() {
            let slot = &SLOTS[index];
            stages.insert(
                (*name).to_string(),
                json!({
                    "calls": slot.calls.load(Ordering::Relaxed),
                    "allocs": slot.allocs.load(Ordering::Relaxed),
                    "alloc_bytes": slot.alloc_bytes.load(Ordering::Relaxed),
                    "reallocs": slot.reallocs.load(Ordering::Relaxed),
                    "dealloc_bytes": slot.dealloc_bytes.load(Ordering::Relaxed),
                    "last_allocs": slot.last_allocs.load(Ordering::Relaxed),
                    "last_alloc_bytes": slot.last_alloc_bytes.load(Ordering::Relaxed),
                }),
            );
        }
        json!({
            "enabled": true,
            "feature": "alloc-profile",
            "capture_active": state.active,
            "requests": state.requests,
            "stages": stages,
            "totals": alloc_profile::snapshot(),
            "note": "per-stage deltas are meaningful for sequential (one request in flight) captures",
        })
    }
}

#[cfg(feature = "alloc-profile")]
pub use imp::{begin_request, mark, reset, snapshot_json};

/// No-op: the default build has no allocation profiler.
#[cfg(not(feature = "alloc-profile"))]
#[inline(always)]
pub fn begin_request() {}

/// No-op: the default build has no allocation profiler.
#[cfg(not(feature = "alloc-profile"))]
#[inline(always)]
pub fn mark(_stage: &'static str) {}

/// No-op: the default build has no allocation profiler.
#[cfg(not(feature = "alloc-profile"))]
#[inline(always)]
pub fn reset() {}

/// `/debug/alloc/stages` in the default build: reports that the feature is off.
#[cfg(not(feature = "alloc-profile"))]
pub fn snapshot_json() -> serde_json::Value {
    serde_json::json!({
        "enabled": false,
        "feature": "alloc-profile",
        "error": "alloc-profile feature is disabled",
    })
}
