//! Optional allocation profiler (feature `alloc-profile`).
//!
//! Counts every alloc/dealloc/realloc and bytes. It is intentionally not part of the default
//! build: atomics on the allocation path distort the numbers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static DEALLOCS: AtomicU64 = AtomicU64::new(0);
pub static REALLOCS: AtomicU64 = AtomicU64::new(0);
pub static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
pub static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCS.fetch_add(1, Ordering::Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

pub fn snapshot() -> serde_json::Value {
    serde_json::json!({
        "allocs": ALLOCS.load(Ordering::Relaxed),
        "deallocs": DEALLOCS.load(Ordering::Relaxed),
        "reallocs": REALLOCS.load(Ordering::Relaxed),
        "alloc_bytes": ALLOC_BYTES.load(Ordering::Relaxed),
        "dealloc_bytes": DEALLOC_BYTES.load(Ordering::Relaxed),
    })
}

pub fn reset() {
    for counter in [&ALLOCS, &DEALLOCS, &REALLOCS, &ALLOC_BYTES, &DEALLOC_BYTES] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn mark(stage: &str) {
    tracing::info!(
        target: "alloc_stage",
        stage,
        allocs = ALLOCS.load(Ordering::Relaxed),
        deallocs = DEALLOCS.load(Ordering::Relaxed),
        reallocs = REALLOCS.load(Ordering::Relaxed),
        alloc_bytes = ALLOC_BYTES.load(Ordering::Relaxed),
        "alloc_stage"
    );
}
