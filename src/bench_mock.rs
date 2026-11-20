//! Ultra-fast in-process mock data-api for the "rail" benchmark.
//!
//! `DL_MOCK_INPROC=<fixtures.json>` replaces the data-api hop with an in-memory table:
//! no socket, no SQL, no fixture hashing. The file maps synthetic dataset ids to canned
//! `fields` / `result` / `pivot` / `distinct` responses; hundreds of "databases" share a
//! template response with different ids.
use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

struct Store {
    fields: HashMap<String, (Value, Value)>,
    result: HashMap<String, Value>,
    pivot: HashMap<String, Value>,
    distinct: HashMap<String, Value>,
}

static STORE: OnceLock<Option<Store>> = OnceLock::new();

fn path() -> Option<String> {
    std::env::var("DL_MOCK_INPROC")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn store() -> Option<&'static Store> {
    STORE
        .get_or_init(|| {
            let file = path()?;
            let text = std::fs::read_to_string(&file).ok()?;
            let root: Value = serde_json::from_str(&text).ok()?;
            let mut fields = HashMap::new();
            let mut result = HashMap::new();
            let mut pivot = HashMap::new();
            let mut distinct = HashMap::new();
            for (ds, body) in root.get("fields")?.as_object()? {
                let f = body.get("fields").cloned().unwrap_or_else(|| Value::Array(vec![]));
                let r = body.get("revision_id").cloned().unwrap_or(Value::Null);
                fields.insert(ds.clone(), (f, r));
            }
            for (ds, body) in root.get("result")?.as_object()? {
                result.insert(ds.clone(), body.clone());
            }
            for (ds, body) in root.get("pivot")?.as_object()? {
                pivot.insert(ds.clone(), body.clone());
            }
            for (ds, body) in root.get("distinct")?.as_object()? {
                distinct.insert(ds.clone(), body.clone());
            }
            Some(Store { fields, result, pivot, distinct })
        })
        .as_ref()
}

pub fn enabled() -> bool {
    path().is_some()
}

pub fn fields(dataset: &str) -> Option<(Value, Value)> {
    store()?.fields.get(dataset).cloned()
}

pub fn result(dataset: &str) -> Option<Value> {
    store()?.result.get(dataset).cloned()
}

pub fn pivot(dataset: &str) -> Option<Value> {
    store()?.pivot.get(dataset).cloned()
}

pub fn distinct(dataset: &str) -> Option<Value> {
    store()?.distinct.get(dataset).cloned()
}
