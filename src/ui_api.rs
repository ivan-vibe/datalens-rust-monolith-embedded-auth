//! Minimal Rust port of the UI API workbook-transfer routes used by meta-manager.
//!
//! ui-api (`APP_MODE=api`) only exposes three internal routes; the full UI does not need
//! them.  Porting them here lets the native cluster drop the second UI-API container while
//! keeping workbook export/import working against the already in-process US/data routers.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};


const UNKNOWN_ENTRY: &str = "UNKNOWN_ENTRY";
const MISSING_LINK: &str = "ERR.UI_API.TRANSFER_MISSING_LINKED_ENTRY";
const INVALID_DATA: &str = "ERR.UI_API.TRANSFER_INVALID_ENTRY_DATA";
const INVALID_SCOPE: &str = "ERR.UI_API.TRANSFER_INVALID_ENTRY_SCOPE";
const INVALID_TOKEN: &str = "ERR.UI_API.TRANSFER_INVALID_TOKEN";

pub fn router() -> Router {
    Router::new()
        .route(
            "/api/internal/v1/workbooks/meta-manager/capabilities/",
            get(capabilities),
        )
        .route(
            "/api/internal/v1/workbooks/meta-manager/capabilities",
            get(capabilities),
        )
        .route("/api/internal/v1/workbooks/export/", post(export))
        .route("/api/internal/v1/workbooks/export", post(export))
        .route("/api/internal/v1/workbooks/import/", post(import))
        .route("/api/internal/v1/workbooks/import", post(import))
}

async fn capabilities() -> Json<Value> {
    Json(json!({
        "dependencies": {
            "connection": [],
            "dataset": ["connection"],
            "widget": ["dataset", "connection"],
            "dash": ["widget", "dataset"]
        }
    }))
}

fn public_base() -> String {
    if let Ok(url) = std::env::var("UI_PUBLIC_URL") {
        return url;
    }
    let port = std::env::var("PUBLIC_PORT").unwrap_or_else(|_| "8099".to_string());
    format!("http://127.0.0.1:{port}")
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    // These calls go to control-api, whose own client already bounds a request at 30 s; a
    // bare `Client::new()` has no deadline at all, so one stalled hop kept a workbook
    // transfer request open forever.
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn control_base() -> String {
    std::env::var("CONTROL_API_URL").unwrap_or_else(|_| "http://127.0.0.1:28084".to_string())
}

fn master_token() -> String {
    std::env::var("US_MASTER_TOKEN").unwrap_or_default()
}

fn valid_transfer_token(headers: &HeaderMap) -> bool {
    headers
        .get("x-us-master-token")
        .and_then(|value| value.to_str().ok())
        .map(|value| value == master_token())
        .unwrap_or(false)
}

fn auth_headers(extra: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&master_token()) {
        out.insert("x-us-master-token", value);
    }
    for name in ["authorization", "cookie", "x-dl-tenantid", "accept-language", "x-request-id"] {
        if let Some(value) = extra.get(name).and_then(|value| value.to_str().ok()) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(value) {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
                    out.insert(name, value);
                }
            }
        }
    }
    out
}

fn us_internal_headers() -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&master_token()) {
        out.insert("x-us-master-token", value);
    }
    out
}

async fn get_entry(id: &str, _headers: &HeaderMap) -> Result<Value, String> {
    let url = format!("{}/private/entries/{}?branch=published", public_base(), id);
    let response = http_client()
        .get(&url)
        .headers(us_internal_headers())
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err("not found".to_string());
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())
}

fn entry_name(key: &str) -> String {
    key.rsplit('/').next().unwrap_or(key).to_string()
}

fn mapping_warning(notifications: &mut Vec<Value>) {
    if !notifications.iter().any(|n| n.get("code") == Some(&json!(MISSING_LINK))) {
        notifications.push(json!({"code": MISSING_LINK, "level": "warning"}));
    }
}

fn map_value(value: &mut Value, id_mapping: &Map<String, Value>, notifications: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(ids)) = object.get_mut("datasetsIds") {
                for id in ids.iter_mut() {
                    if let Some(text) = id.as_str() {
                        if let Some(mapped) = id_mapping.get(text).and_then(Value::as_str) {
                            *id = json!(mapped);
                        } else {
                            *id = json!(UNKNOWN_ENTRY);
                            mapping_warning(notifications);
                        }
                    }
                }
            }
            if let Some(mapped_id) = object.get("datasetId").and_then(Value::as_str).map(str::to_string) {
                if let Some(mapped) = id_mapping.get(&mapped_id).and_then(Value::as_str) {
                    object.insert("datasetId".to_string(), json!(mapped));
                } else {
                    object.insert("datasetId".to_string(), json!(UNKNOWN_ENTRY));
                    mapping_warning(notifications);
                }
            }
            if let Some(connection_id) = object.get("connection_id").and_then(Value::as_str).map(str::to_string) {
                if let Some(mapped) = id_mapping.get(&connection_id).and_then(Value::as_str) {
                    object.insert("connection_id".to_string(), json!(mapped));
                } else {
                    object.insert("connection_id".to_string(), json!(UNKNOWN_ENTRY));
                    mapping_warning(notifications);
                }
            }
            for (_, child) in object.iter_mut() {
                map_value(child, id_mapping, notifications);
            }
        }
        Value::Array(items) => {
            for item in items {
                map_value(item, id_mapping, notifications);
            }
        }
        _ => {}
    }
}

fn map_ql_connection(shared: &mut Value, id_mapping: &Map<String, Value>, notifications: &mut Vec<Value>) {
    if let Some(connection) = shared.get_mut("connection").and_then(Value::as_object_mut) {
        if let Some(entry_id) = connection.get("entryId").and_then(Value::as_str).map(str::to_string) {
            if let Some(mapped) = id_mapping.get(&entry_id).and_then(Value::as_str) {
                connection.insert("entryId".to_string(), json!(mapped));
            } else {
                mapping_warning(notifications);
            }
        }
    }
}

fn process_dash_links(
    value: &mut Value,
    id_mapping: &Map<String, Value>,
    notifications: &mut Vec<Value>,
    export: bool,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if key == "chartId" || key == "datasetId" || key == "entryId" {
                    if let Some(old) = child.as_str().map(str::to_string) {
                        if let Some(mapped) = id_mapping.get(&old).and_then(Value::as_str) {
                            *child = json!(mapped);
                        } else {
                            mapping_warning(notifications);
                            if export {
                                *child = json!(UNKNOWN_ENTRY);
                            }
                        }
                    }
                } else {
                    process_dash_links(child, id_mapping, notifications, export);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                process_dash_links(item, id_mapping, notifications, export);
            }
        }
        _ => {}
    }
}

fn export_widget(entry: &Value, id_mapping: &Map<String, Value>) -> (Value, Vec<Value>) {
    let mut notifications = Vec::new();
    let Some(shared_text) = entry.pointer("/data/shared").and_then(Value::as_str) else {
        return (Value::Null, vec![json!({"code": INVALID_DATA, "level": "critical"})]);
    };
    let Ok(mut shared) = serde_json::from_str::<Value>(shared_text) else {
        return (Value::Null, vec![json!({"code": INVALID_DATA, "level": "critical"})]);
    };
    match shared.get("type").and_then(Value::as_str) {
        Some("datalens") => map_value(&mut shared, id_mapping, &mut notifications),
        Some("ql") => map_ql_connection(&mut shared, id_mapping, &mut notifications),
        _ => return (Value::Null, vec![json!({"code": INVALID_DATA, "level": "critical"})]),
    }
    let widget = json!({
        "name": entry_name(entry.get("key").and_then(Value::as_str).unwrap_or("")),
        "type": entry.get("type").cloned().unwrap_or(Value::Null),
        "annotation": entry.get("annotation").cloned().unwrap_or_else(|| json!({})),
        "data": {"shared": shared},
    });
    (widget, notifications)
}

fn export_dash(entry: &Value, id_mapping: &Map<String, Value>) -> (Value, Vec<Value>) {
    let mut notifications = Vec::new();
    let mut data = entry.get("data").cloned().unwrap_or(Value::Null);
    process_dash_links(&mut data, id_mapping, &mut notifications, true);
    let dash = json!({
        "name": entry_name(entry.get("key").and_then(Value::as_str).unwrap_or("")),
        "data": data,
        "annotation": entry.get("annotation").cloned().unwrap_or_else(|| json!({})),
    });
    (dash, notifications)
}

fn error_response(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"code": code}))).into_response()
}

async fn export(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !valid_transfer_token(&headers) {
        return error_response(StatusCode::FORBIDDEN, INVALID_TOKEN);
    }
    let scope = body.get("scope").and_then(Value::as_str).unwrap_or("");
    let export_id = body.get("exportId").and_then(Value::as_str).unwrap_or("");
    let id_mapping = body.get("idMapping").and_then(Value::as_object).cloned().unwrap_or_default();
    let workbook_id = body.get("workbookId").and_then(Value::as_str).unwrap_or("");
    let result = match scope {
        "widget" => get_entry(export_id, &headers).await.map(|entry| {
            let (widget, notifications) = export_widget(&entry, &id_mapping);
            json!({"notifications": notifications, "entryData": {"widget": widget}})
        }),
        "dash" => get_entry(export_id, &headers).await.map(|entry| {
            let (dash, notifications) = export_dash(&entry, &id_mapping);
            json!({"notifications": notifications, "entryData": {"dash": dash}})
        }),
        "connection" => export_connection(export_id, workbook_id, &headers).await,
        "dataset" => export_dataset(export_id, workbook_id, &id_mapping, &headers).await,
        _ => return error_response(StatusCode::OK, INVALID_SCOPE),
    };
    match result {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"code": error}))).into_response(),
    }
}

async fn export_connection(id: &str, _workbook_id: &str, headers: &HeaderMap) -> Result<Value, String> {
    let url = format!("{}/api/v1/connections/export/{}", control_base(), id);
    let response = http_client()
        .get(&url)
        .headers(auth_headers(headers))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value = response.json::<Value>().await.map_err(|error| error.to_string())?;
    let connection = value.get("connection").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "notifications": value.get("notifications").cloned().unwrap_or_else(|| json!([])),
        "entryData": if connection.is_null() { json!({}) } else { json!({"connection": connection}) },
    }))
}

async fn export_dataset(id: &str, _workbook_id: &str, id_mapping: &Map<String, Value>, headers: &HeaderMap) -> Result<Value, String> {
    let url = format!("{}/api/v1/datasets/export/{}", control_base(), id);
    let response = http_client()
        .post(&url)
        .headers(auth_headers(headers))
        .json(&json!({"id_mapping": id_mapping}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value = response.json::<Value>().await.map_err(|error| error.to_string())?;
    let dataset = value.get("dataset").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "notifications": value.get("notifications").cloned().unwrap_or_else(|| json!([])),
        "entryData": if dataset.is_null() { json!({}) } else { json!({"dataset": dataset}) },
    }))
}

// Import is intentionally added after the export path is verified against the old ui-api;
// the route stays registered so meta-manager can be moved without a second rollout.

async fn import(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if !valid_transfer_token(&headers) {
        return error_response(StatusCode::FORBIDDEN, INVALID_TOKEN);
    }
    let entry_data = body.get("entryData").cloned().unwrap_or(Value::Null);
    let workbook_id = body.get("workbookId").and_then(Value::as_str).unwrap_or("");
    let id_mapping = body.get("idMapping").and_then(Value::as_object).cloned().unwrap_or_default();
    let scope = if entry_data.get("widget").is_some() {
        "widget"
    } else if entry_data.get("dash").is_some() {
        "dash"
    } else if entry_data.get("connection").is_some() {
        "connection"
    } else if entry_data.get("dataset").is_some() {
        "dataset"
    } else {
        ""
    };
    let result = match scope {
        "widget" => import_widget(&entry_data["widget"], workbook_id, &id_mapping, &headers).await,
        "dash" => import_dash(&entry_data["dash"], workbook_id, &id_mapping, &headers).await,
        "connection" => import_connection(&entry_data["connection"], workbook_id, &headers).await,
        "dataset" => import_dataset(&entry_data["dataset"], workbook_id, &id_mapping, &headers).await,
        _ => return error_response(StatusCode::OK, INVALID_SCOPE),
    };
    match result {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"code": error}))).into_response(),
    }
}

async fn create_us_entry(body: Value, workbook_id: &str, _headers: &HeaderMap) -> Result<String, String> {
    let url = format!("{}/private/entries/", public_base());
    let response = http_client()
        .post(&url)
        .headers(us_internal_headers())
        .header("x-dl-workbookid", workbook_id)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value = response.json::<Value>().await.map_err(|error| error.to_string())?;
    value
        .get("entryId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("create entry failed: {value}"))
}

async fn import_widget(widget: &Value, workbook_id: &str, id_mapping: &Map<String, Value>, headers: &HeaderMap) -> Result<Value, String> {
    let mut shared = widget.pointer("/data/shared").cloned().unwrap_or(Value::Null);
    map_value(&mut shared, id_mapping, &mut Vec::new());
    let shared_text = serde_json::to_string(&shared).map_err(|error| error.to_string())?;
    let body = json!({
        "workbookId": workbook_id,
        "name": widget.get("name").cloned().unwrap_or(Value::Null),
        "type": widget.get("type").cloned().unwrap_or(Value::Null),
        "scope": "widget",
        "mode": "publish",
        "links": {},
        "data": {"shared": shared_text},
        "annotation": {
            "description": widget.pointer("/annotation/description").and_then(Value::as_str).unwrap_or("")
        },
    });
    let id = create_us_entry(body, workbook_id, headers).await?;
    Ok(json!({"id": id, "notifications": []}))
}

async fn import_dash(dash: &Value, workbook_id: &str, id_mapping: &Map<String, Value>, headers: &HeaderMap) -> Result<Value, String> {
    let mut data = dash.get("data").cloned().unwrap_or(Value::Null);
    process_dash_links(&mut data, id_mapping, &mut Vec::new(), false);
    let body = json!({
        "workbookId": workbook_id,
        "name": dash.get("name").cloned().unwrap_or(Value::Null),
        "type": "dash",
        "scope": "dash",
        "mode": "publish",
        "links": {},
        "data": data,
        "annotation": {
            "description": dash.pointer("/annotation/description").and_then(Value::as_str).unwrap_or("")
        },
    });
    let id = create_us_entry(body, workbook_id, headers).await?;
    Ok(json!({"id": id, "notifications": []}))
}

async fn import_connection(connection: &Value, workbook_id: &str, headers: &HeaderMap) -> Result<Value, String> {
    let url = format!("{}/api/v1/connections/import", control_base());
    let response = http_client()
        .post(&url)
        .headers(auth_headers(headers))
        .json(&json!({"data": {"workbook_id": workbook_id, "connection": connection}, "id_mapping": {}}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value = response.json::<Value>().await.map_err(|error| error.to_string())?;
    Ok(json!({"id": value.get("id").cloned().unwrap_or(Value::Null), "notifications": value.get("notifications").cloned().unwrap_or_else(|| json!([]))}))
}

async fn import_dataset(dataset: &Value, workbook_id: &str, id_mapping: &Map<String, Value>, headers: &HeaderMap) -> Result<Value, String> {
    let url = format!("{}/api/v1/datasets/import", control_base());
    let response = http_client()
        .post(&url)
        .headers(auth_headers(headers))
        .json(&json!({"data": {"workbook_id": workbook_id, "dataset": dataset}, "id_mapping": id_mapping}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value = response.json::<Value>().await.map_err(|error| error.to_string())?;
    Ok(json!({"id": value.get("id").cloned().unwrap_or(Value::Null), "notifications": value.get("notifications").cloned().unwrap_or_else(|| json!([]))}))
}
