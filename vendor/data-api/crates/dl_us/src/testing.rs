//! A mock `us` for tests: one real-shaped dataset entry plus a server that serves it.
//!
//! Every request is recorded, so a test can assert what the client actually sent: the
//! `branch` query parameter, the `X-US-Master-Token` / `Authorization` header and the
//! `X-DL-Component` header.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

/// A dataset entry with a dimension, a measure, a formula and a parameter.
///
/// The shape and the values follow the demo dataset shipped with DataLens, so a mapping
/// bug shows up here rather than in a running container.
pub fn dataset_entry() -> Value {
    json!({
        "entryId": "dataset-1",
        "revId": "rev-1",
        "scope": "dataset",
        "type": "dataset",
        "key": "demo/sales",
        "data": {
            "name": "Sales",
            "result_schema": [
                {
                    "guid": "dim_region",
                    "title": "Region",
                    "type": "DIMENSION",
                    "calc_mode": "direct",
                    "source": "region",
                    "avatar_id": "avatar_orders",
                    "data_type": "string",
                    "initial_data_type": "string",
                    "cast": "string",
                    "aggregation": "none",
                    "hidden": false,
                    "valid": true
                },
                {
                    "guid": "m_sales",
                    "title": "Sales",
                    "type": "MEASURE",
                    "calc_mode": "direct",
                    "source": "amount",
                    "avatar_id": "avatar_orders",
                    "data_type": "float",
                    "initial_data_type": "float",
                    "cast": "float",
                    "aggregation": "sum",
                    "hidden": false,
                    "valid": true
                },
                {
                    "guid": "f_double",
                    "title": "Double",
                    "type": "MEASURE",
                    "calc_mode": "formula",
                    "source": "",
                    "avatar_id": null,
                    "formula": "[Sales] * 2",
                    "guid_formula": "[m_sales] * 2",
                    "data_type": "float",
                    "initial_data_type": "float",
                    "cast": "float",
                    "aggregation": "none",
                    "hidden": false,
                    "valid": true
                },
                {
                    "guid": "p_scale",
                    "title": "scale",
                    "type": "DIMENSION",
                    "calc_mode": "parameter",
                    "source": "",
                    "avatar_id": null,
                    "formula": "",
                    "guid_formula": "",
                    "data_type": "float",
                    "initial_data_type": "float",
                    "cast": "float",
                    "aggregation": "none",
                    "hidden": false,
                    "valid": true,
                    "default_value": {"type": "float", "value": 0.5}
                }
            ],
            "source_collections": [
                {
                    "id": "coll_orders",
                    "type": "collection",
                    "title": "orders",
                    "valid": true,
                    "origin": {
                        "connection_id": "conn-1",
                        "parameters": {"table_name": "orders", "schema_name": "public"}
                    }
                }
            ],
            "source_avatars": [
                {
                    "id": "avatar_orders",
                    "title": "public.orders",
                    "source_id": "coll_orders",
                    "is_root": true,
                    "valid": true
                }
            ],
            "avatar_relations": [],
            "rls": []
        }
    })
}

/// What one client call looked like, as the mock `us` saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    pub entry_id: String,
    pub branch: Option<String>,
    pub master_token: Option<String>,
    pub authorization: Option<String>,
    pub component: Option<String>,
}

#[derive(Clone)]
struct MockState {
    entries: Arc<HashMap<String, Value>>,
    log: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// A mock `us` server on a random local port.
pub struct MockUs {
    base_url: String,
    log: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockUs {
    /// Serve `entries` (`entry id -> entry JSON`) until the process ends.
    pub async fn start(entries: HashMap<String, Value>) -> Self {
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            entries: Arc::new(entries),
            log: log.clone(),
        };
        let router = Router::new()
            .route("/private/entries/{entry_id}", get(handle_entry))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a mock us");
        let address = listener.local_addr().expect("mock us address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base_url: format!("http://{address}"),
            log,
        }
    }

    /// A mock serving [`dataset_entry`] as `dataset-1`.
    pub async fn with_dataset() -> Self {
        let mut entries = HashMap::new();
        entries.insert("dataset-1".to_string(), dataset_entry());
        Self::start(entries).await
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.log.lock().expect("mock us log").clone()
    }
}

async fn handle_entry(
    State(state): State<MockState>,
    Path(entry_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    state
        .log
        .lock()
        .expect("mock us log")
        .push(RecordedRequest {
            entry_id: entry_id.clone(),
            branch: query.get("branch").cloned(),
            master_token: header("x-us-master-token"),
            authorization: header("authorization"),
            component: header("x-dl-component"),
        });
    match state.entries.get(&entry_id) {
        Some(entry) => (StatusCode::OK, Json(entry.clone())),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"code": "US_ENTRY_NOT_FOUND", "message": "no such entry"})),
        ),
    }
}
