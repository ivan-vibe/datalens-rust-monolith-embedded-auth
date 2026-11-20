//! Idle memory trimmer.
//!
//! mimalloc keeps freed pages in its arenas and the render caches keep parsed chart metadata
//! alive, so a process that served a dashboard stays at ~90 MB even when no request has
//! arrived for minutes. `MEMORY_IDLE_TRIM_SECS=<n>` (0 = off) drops the render caches and calls
//! `mi_collect(true)` once the process has been quiet for `n` seconds, returning the pages to
//! the OS. The caches are rebuilt on the next request; only the first chart after a long pause
//! pays the re-parse.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(all(
    not(feature = "system-alloc"),
    not(all(target_os = "macos", not(feature = "mimalloc-alloc")))
))]
extern "C" {
    /// `libmimalloc` C API: eagerly return free pages to the OS (`force = true`).
    fn mi_collect(force: bool);
}

static LAST_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark the process active (called by the chart handlers, which dominate allocations).
pub fn touch() {
    LAST_ACTIVITY_MS.store(now_ms(), Ordering::Relaxed);
}

fn idle_ms() -> u64 {
    let last = LAST_ACTIVITY_MS.load(Ordering::Relaxed);
    if last == 0 {
        0
    } else {
        now_ms().saturating_sub(last)
    }
}

/// Background trimmer: every 5 s check the idle time; on a quiet period clear the render caches
/// and force mimalloc to return pages. `idle_secs == 0` disables it.
pub async fn idle_trimmer(idle_secs: u64) {
    if idle_secs == 0 {
        return;
    }
    let threshold_ms = idle_secs.saturating_mul(1000);
    let mut last_trim_ms = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let idle = idle_ms();
        let now = now_ms();
        if idle >= threshold_ms && now.saturating_sub(last_trim_ms) >= 10_000 {
            last_trim_ms = now;
            let dropped = crate::pipeline::clear_idle_caches();
            let entries = crate::us_direct::clear_entry_cache();
            // `mi_collect` is the C API of the mimalloc static library that `mimalloc` already
            // links as the global allocator; calling it needs no extra dependency.
            #[cfg(all(
                not(feature = "system-alloc"),
                not(all(target_os = "macos", not(feature = "mimalloc-alloc")))
            ))]
            unsafe {
                mi_collect(true);
            }
            tracing::info!(
                idle_secs = idle / 1000,
                render_caches = dropped,
                entry_cache = entries,
                "idle memory trim: caches cleared, mimalloc collected"
            );
        }
    }
}
