//! Compile a stored `us` dataset + a `/result` body and print the SQL.
//!
//!   cargo run --example dump_sql -- <us-entry.json> <request.json>
//!
//! The entry JSON is what `GET /private/entries/{id}` answers; the request JSON is what the UI
//! posts to `/api/data/v2/datasets/{id}/result`. Nothing is executed: the point is to read the
//! SQL the way `--dry-run` would print it.
use datalens_server::AnyExecutor;
use dl_api::Service;
use dl_query::Dialect;
use dl_us::{to_dataset, UsEntry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let entry_path = args
        .next()
        .ok_or("usage: dump_sql <us-entry.json> <request.json>")?;
    let request_path = args
        .next()
        .ok_or("usage: dump_sql <us-entry.json> <request.json>")?;

    let entry: UsEntry = serde_json::from_str(&std::fs::read_to_string(&entry_path)?)?;
    let dataset = to_dataset(&entry.entry_id, &entry.dataset_data()?)?;
    let request: dl_api::ResultRequest =
        serde_json::from_str(&std::fs::read_to_string(&request_path)?)?;

    let service = Service::new(AnyExecutor::postgres(None), Dialect::Postgres);
    let prepared = service.prepare_result(&dataset, &request)?;
    for block in &prepared.blocks {
        println!("-- block {} ({:?})", block.block_id, block.placement);
        println!("{}", block.query.sql);
    }
    if let Some(totals) = prepared.totals {
        println!("-- totals: {}", totals.query.sql);
    }
    Ok(())
}
