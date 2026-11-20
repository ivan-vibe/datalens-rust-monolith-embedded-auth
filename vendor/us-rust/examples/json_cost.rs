//! Measures JSON parse/serialize cost for the demo dashboard payload.
//!
//!   cargo run --release --example json_cost -- /tmp/dash_data.json
//!
//! `simd-json` is a dev-dependency: this example compares it with `serde_json`
//! on the exact payload the US service sends.

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("path to a JSON file");
    let bytes = std::fs::read(&path).expect("read json");
    println!("payload: {} bytes", bytes.len());

    let iterations = 300;

    // serde_json parse
    let started = Instant::now();
    for _ in 0..iterations {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        std::hint::black_box(&value);
    }
    let parse = started.elapsed().as_secs_f64() / iterations as f64 * 1e6;
    println!(
        "serde_json parse: {parse:8.1} us  ({:.0} MB/s)",
        bytes.len() as f64 / parse / 1.0
    );

    // serde_json serialize
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let started = Instant::now();
    for _ in 0..iterations {
        let out = serde_json::to_vec(&value).unwrap();
        std::hint::black_box(&out);
    }
    let ser = started.elapsed().as_secs_f64() / iterations as f64 * 1e6;
    println!(
        "serde_json serialize: {ser:8.1} us  ({:.0} MB/s)",
        bytes.len() as f64 / ser
    );

    // parse + serialize, the cost of one DB jsonb round-trip in the API
    let started = Instant::now();
    for _ in 0..iterations {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let out = serde_json::to_vec(&value).unwrap();
        std::hint::black_box(&out);
    }
    let both = started.elapsed().as_secs_f64() / iterations as f64 * 1e6;
    println!("serde_json parse+serialize: {both:8.1} us");

    // simd-json parse (owned value)
    let started = Instant::now();
    for _ in 0..iterations {
        let mut buffer = bytes.clone();
        let value = simd_json::to_owned_value(&mut buffer).unwrap();
        std::hint::black_box(&value);
    }
    let simd = started.elapsed().as_secs_f64() / iterations as f64 * 1e6;
    println!(
        "simd_json parse: {simd:8.1} us  ({:.0} MB/s)",
        bytes.len() as f64 / simd
    );

    // simd-json parse + simd-json serialize
    let started = Instant::now();
    for _ in 0..iterations {
        let mut buffer = bytes.clone();
        let value = simd_json::to_owned_value(&mut buffer).unwrap();
        let out = simd_json::serde::to_string(&value).unwrap();
        std::hint::black_box(&out);
    }
    let simd_both = started.elapsed().as_secs_f64() / iterations as f64 * 1e6;
    println!("simd_json parse+serialize: {simd_both:8.1} us");
    println!(
        "=> simd_json saves {:.0}% of parse, which is {:.1}% of one parse+serialize round-trip",
        (1.0 - simd / parse) * 100.0,
        (parse - simd) / both * 100.0
    );
}
