//! Text `simple_query` vs binary extended protocol on the same demo table.
//!
//! Run inside the compose network:
//!   cargo bench -p dl_postgres --bench binary
//! with DL_TEST_POSTGRES_DSN=postgres://pg-user:postgres@postgres:5432/pg-demo-db

use std::time::Instant;

const ITERATIONS: usize = 20;

fn pct(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[((values.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let dsn = std::env::var("DL_TEST_POSTGRES_DSN")
        .unwrap_or_else(|_| "postgres://pg-user:postgres@postgres:5432/pg-demo-db".to_string());
    let (client, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // Warm both paths and the shared buffers.
    for _ in 0..3 {
        let _ = client
            .simple_query("SELECT order_buyer_region_name, amount::float8 FROM public.orders")
            .await;
        let _ = client
            .query(
                "SELECT order_buyer_region_name, amount::float8 FROM public.orders",
                &[],
            )
            .await;
    }

    let sql = "SELECT order_buyer_region_name, amount::float8 FROM public.orders";
    let mut text_times = Vec::new();
    let mut binary_times = Vec::new();
    let mut text_sum = 0.0;
    let mut binary_sum = 0.0;
    let mut rows = 0;

    for _ in 0..ITERATIONS {
        let started = Instant::now();
        let messages = client.simple_query(sql).await.expect("text query");
        let mut local = 0.0;
        let mut count = 0usize;
        for message in messages {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = message {
                local += row.get(1).unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                count += 1;
            }
        }
        text_times.push(started.elapsed().as_secs_f64() * 1000.0);
        text_sum += local;
        rows = count;

        let started = Instant::now();
        let rows_binary = client.query(sql, &[]).await.expect("binary query");
        let mut local = 0.0;
        for row in &rows_binary {
            let _region: &str = row.get(0);
            local += row.get::<_, f64>(1);
        }
        binary_times.push(started.elapsed().as_secs_f64() * 1000.0);
        binary_sum += local;
    }

    let mut text_times = text_times;
    let mut binary_times = binary_times;
    println!(
        "rows={rows} text p50={:.1}ms p95={:.1}ms binary p50={:.1}ms p95={:.1}ms",
        pct(&mut text_times, 0.5),
        pct(&mut text_times, 0.95),
        pct(&mut binary_times, 0.5),
        pct(&mut binary_times, 0.95)
    );
    assert!((text_sum - binary_sum).abs() < 0.01);
}
