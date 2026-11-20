//! JSON vs MessagePack for a representative result document.
//!
//! This isolates the codec: same `serde_json::Value`, no HTTP, no database.
//! `cargo bench -p dl_api --bench codec`.

use std::hint::black_box;
use std::time::Instant;

use serde_json::{json, Value};

const ROWS: usize = 10_000;
const ITERATIONS: usize = 200;

fn document() -> Value {
    let rows: Vec<Value> = (0..ROWS)
        .map(|index| {
            json!([
                format!("region-{}", index % 20),
                index as f64 * 1.25,
                format!("2020-01-{:02}T00:00:00", index % 28 + 1),
                format!("client-{}", index % 1000)
            ])
        })
        .collect();
    json!({
        "fields": [
            {"legend_item_id": 0, "id": "f_region", "title": "Region", "data_type": "string", "field_type": "DIMENSION", "item_type": "field", "role_spec": {"role": "row", "visibility": "visible"}},
            {"legend_item_id": 1, "id": "f_sales", "title": "Sales", "data_type": "float", "field_type": "MEASURE", "item_type": "field", "role_spec": {"role": "measure", "visibility": "visible"}},
            {"legend_item_id": 2, "id": "f_date", "title": "Date", "data_type": "date", "field_type": "DIMENSION", "item_type": "field", "role_spec": {"role": "row", "visibility": "visible"}},
            {"legend_item_id": 3, "id": "f_client", "title": "Client", "data_type": "string", "field_type": "DIMENSION", "item_type": "field", "role_spec": {"role": "row", "visibility": "visible"}}
        ],
        "result_data": [{"rows": rows}],
        "blocks": [{"block_id": 0, "query": "SELECT ..."}],
        "notifications": []
    })
}

fn us(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn main() {
    let value = document();
    let json = serde_json::to_vec(&value).unwrap();
    let msgpack = rmp_serde::to_vec_named(&value).unwrap();
    println!(
        "rows={ROWS} json={} B msgpack={} B ({:.1}% of json)",
        json.len(),
        msgpack.len(),
        msgpack.len() as f64 * 100.0 / json.len() as f64
    );

    for name in ["json", "msgpack"] {
        let mut encode = 0.0;
        let mut decode = 0.0;
        for _ in 0..10 {
            let started = Instant::now();
            let bytes = if name == "json" {
                serde_json::to_vec(&value).unwrap()
            } else {
                rmp_serde::to_vec_named(&value).unwrap()
            };
            encode += us(started.elapsed());
            let started = Instant::now();
            if name == "json" {
                black_box(serde_json::from_slice::<Value>(&bytes).unwrap());
            } else {
                black_box(rmp_serde::from_slice::<Value>(&bytes).unwrap());
            }
            decode += us(started.elapsed());
        }
        let mut encode_total = 0.0;
        let mut decode_total = 0.0;
        for _ in 0..ITERATIONS {
            let started = Instant::now();
            let bytes = if name == "json" {
                serde_json::to_vec(&value).unwrap()
            } else {
                rmp_serde::to_vec_named(&value).unwrap()
            };
            encode_total += us(started.elapsed());
            let started = Instant::now();
            if name == "json" {
                black_box(serde_json::from_slice::<Value>(&bytes).unwrap());
            } else {
                black_box(rmp_serde::from_slice::<Value>(&bytes).unwrap());
            }
            decode_total += us(started.elapsed());
        }
        println!(
            "{name:<8} encode {:>9.1} us  decode {:>9.1} us  ({} iterations)",
            encode_total / ITERATIONS as f64,
            decode_total / ITERATIONS as f64,
            ITERATIONS
        );
    }
}
