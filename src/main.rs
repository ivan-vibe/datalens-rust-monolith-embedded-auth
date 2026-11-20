#[cfg(feature = "alloc-profile")]
mod alloc_profile;
#[cfg(feature = "alloc-profile")]
#[global_allocator]
static ALLOC_PROFILE: alloc_profile::Counting = alloc_profile::Counting;

/// Process-wide allocator. The charts path is allocation-heavy (serde_json
/// `Value`s, request/response buffers, per-request format strings), and musl's
/// built-in allocator is slower than mimalloc on that mixed small-allocation
/// pattern. `alloc-profile` builds keep the counting allocator instead: it has
/// to observe every allocation, so the two are mutually exclusive.
/// Allocator selection:
/// * macOS default = system malloc: with 4 load processes x 16 clients it matched mimalloc's
///   RPS/p50/p95 and kept 30-60 MB less (mimalloc arenas dominate `vmmap`'s IOAccelerator
///   category); see `bench/rail/MEMORY.md` (phase 4).
/// * other targets default = mimalloc (the original musl throughput choice).
/// * `system-alloc` / `mimalloc-alloc` force one allocator for A/B builds.
#[cfg(all(
    not(feature = "alloc-profile"),
    any(
        feature = "system-alloc",
        all(target_os = "macos", not(feature = "mimalloc-alloc"))
    )
))]
#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;

#[cfg(all(
    not(feature = "alloc-profile"),
    not(any(
        feature = "system-alloc",
        all(target_os = "macos", not(feature = "mimalloc-alloc"))
    ))
))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod d3_area;
mod bench_mock;
mod data_direct;
mod demo_seed;
mod data_main;
mod engine_main;
mod memory;
mod connections;
mod control_schemas;
mod control_api;
mod meta_api;
mod workbook_queue;
mod meta_manager;
mod geo_engine;
mod gravity_bar;
mod gravity_line;
mod gravity_scatter;
mod gravity_tree;
mod gravity_donut;
mod graph_engine;
mod js_format;
mod local_us;
mod local_http;
pub mod msgpack;
pub mod pipeline;
mod pivot_engine;
mod typed_data;
mod proxy;
mod raw_envelope;
mod us_direct;
mod ui_api;
mod us_main;

fn probe(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    if stream.write_all(b"GET /ping HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let Ok(n) = stream.read(&mut buf) else { return false; };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

fn healthcheck() -> i32 {
    // `PUBLIC_PORT=0` means "let the process pick a free port": in that mode the launcher
    // reads the bound port back from `DL_PORTS_FILE`, so the healthcheck does the same.
    let env_port = |name: &str, default: &str| -> u16 {
        std::env::var(name).unwrap_or_else(|_| default.into()).parse().unwrap_or(0)
    };
    // A launcher records the bound port in `DL_PORTS_FILE` (the "port from the process"
    // contract); read it first, then fall back to `PUBLIC_PORT` for container deployments.
    let mut public = 0;
    let file = std::env::var("DL_PORTS_FILE").unwrap_or_default();
    if let Ok(body) = std::fs::read_to_string(&file) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(port) = value.get("public").and_then(|port| port.as_u64()) {
                public = port as u16;
            }
        }
    }
    if public == 0 {
        public = env_port("PUBLIC_PORT", "8080");
    }
    if probe(public) { 0 } else { 1 }
}

fn main() {
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    // One OS process, one container. Each service keeps its existing listener and runtime;
    // the internal URLs point at loopback ports of this same process.
    let dl_port = std::env::var("DL_PORT").unwrap_or_else(|_| "8081".into());
    let us_port = std::env::var("APP_PORT").unwrap_or_else(|_| "8090".into());
    let engine_port = std::env::var("ENGINE_PORT").unwrap_or_else(|_| "8089".into());
    std::env::set_var("DL_PORT", &dl_port);
    std::env::set_var("APP_PORT", &us_port);
    std::env::set_var("PORT", &us_port);
    std::env::set_var("ENGINE_PORT", &engine_port);
    std::env::set_var("DATA_API_URL", format!("http://127.0.0.1:{dl_port}"));
    std::env::set_var("US_URL", format!("http://127.0.0.1:{us_port}"));
    std::env::set_var("US_HOST", "inproc://us");

    // Fork: auth is served from this process unless explicitly disabled. Its keys are required
    // for both signing (login) and the UI's token validation.
    let embedded_auth = std::env::var("AUTH_EMBEDDED")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(true);
    let auth_port = std::env::var("AUTH_PORT").unwrap_or_else(|_| "8097".into());
    std::env::set_var("AUTH_PORT", &auth_port);
    if embedded_auth {
        for key in ["TOKEN_PRIVATE_KEY", "TOKEN_PUBLIC_KEY"] {
            if std::env::var(key)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
            {
                eprintln!("fatal: {key} must be set for the embedded auth service");
                std::process::exit(2);
            }
        }
    }

    // Fail closed on a misconfigured auth profile. Before this check, an unset AUTH_ENABLED
    // (or AUTH__TYPE=NONE with a token-less US) left the public API and the master-token
    // routes open while the process looked healthy. A monolith deployment has to say either
    // "auth is on and fully configured" or "auth is explicitly off".
    if std::env::var("MONOLITH").is_ok() {
        let auth_enabled = std::env::var("AUTH_ENABLED")
            .map(|value| value == "true" || value == "1")
            .unwrap_or(true);
        if auth_enabled {
            let auth_type = std::env::var("AUTH__TYPE").unwrap_or_default();
            if auth_type.trim().to_uppercase() != "NATIVE" {
                eprintln!(
                    "fatal: AUTH_ENABLED is on but AUTH__TYPE={auth_type:?}; set AUTH__TYPE=NATIVE \
                     (or AUTH_ENABLED=0 for a local, unauthenticated run)"
                );
                std::process::exit(2);
            }
            for key in ["US_MASTER_TOKEN", "AUTH__JWT_KEY"] {
                if std::env::var(key).map(|value| value.trim().is_empty()).unwrap_or(true) {
                    eprintln!("fatal: {key} must be set when AUTH_ENABLED is on");
                    std::process::exit(2);
                }
            }
        } else {
            eprintln!(
                "warning: AUTH_ENABLED=0 — the public API is unauthenticated (local profile only)"
            );
        }
    }

    // v8: ONE tokio runtime for the whole process.
    //
    // Before this, data-api / us / charts-engine each built their own multi-thread runtime
    // on their own thread, so the process ran four runtimes (and the `--dry-run` data-api
    // kept a whole idle runtime alive for the benchmark). They now share this runtime's
    // worker pool; the three helper threads only drive their startup future with
    // `Handle::block_on`, which blocks the thread (not a runtime worker) and keeps the
    // service entrypoints free of `Send`/'static requirements.
    let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
    runtime_builder.enable_all();
    // The whole monolith shares one runtime. A concurrency level above the DB pool makes the
    // charts queue on the backend, so the worker count is tunable for A/B measurement.
    if let Some(workers) = std::env::var("TOKIO_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|workers| *workers > 0)
    {
        runtime_builder.worker_threads(workers);
    }
    if let Some(blocking) = std::env::var("TOKIO_MAX_BLOCKING_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|blocking| *blocking > 0)
    {
        runtime_builder.max_blocking_threads(blocking);
    }
    let runtime = runtime_builder.build().expect("service runtime");
    if embedded_auth {
        let bind = std::env::var("AUTH_BIND").unwrap_or_else(|_| "127.0.0.1".into());
        let port = auth_port.clone();
        eprintln!("embedded auth: listening on {bind}:{port} (single-binary fork)");
        runtime.spawn(async move {
            if let Err(error) = datalens_auth_rs::serve(&bind, std::future::pending()).await {
                eprintln!("fatal: embedded auth failed: {error:#}");
                std::process::exit(2);
            }
        });
    }
    if let Some(idle_secs) = std::env::var("MEMORY_IDLE_TRIM_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
    {
        runtime.spawn(crate::memory::idle_trimmer(idle_secs));
        eprintln!("idle memory trim enabled: {idle_secs}s");
    }
    let _local_us = runtime.block_on(async { local_us::start() });

    let data = std::thread::Builder::new()
        .name("data-api".to_string())
        .spawn({
            let handle = runtime.handle().clone();
            move || { let _ = handle.block_on(data_main::run()); }
        })
        .expect("spawn data-api");
    let us = std::thread::Builder::new()
        .name("us".to_string())
        .spawn({
            let handle = runtime.handle().clone();
            move || { let _ = handle.block_on(us_main::run()); }
        })
        .expect("spawn us");
    let engine = std::thread::Builder::new()
        .name("charts-engine".to_string())
        .spawn({
            let handle = runtime.handle().clone();
            move || { handle.block_on(engine_main::run()); }
        })
        .expect("spawn charts-engine");

    let public = std::env::var("PUBLIC_PORT").unwrap_or_else(|_| "8080".into()).parse().unwrap_or(8080);
    let dl: u16 = dl_port.parse().unwrap_or(8081);
    let usp: u16 = us_port.parse().unwrap_or(8090);
    let eng: u16 = engine_port.parse().unwrap_or(8089);
    let _ = runtime.block_on(proxy::serve(public, dl, usp, eng));
    let _ = data.join();
    let _ = us.join();
    let _ = engine.join();
}
