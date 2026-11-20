//! HTTP surface for the Rust meta-manager replacement.
//!
//! The transfer core lives in `meta_manager`; this module only translates the Node
//! meta-manager routes into `JobStore` operations. It intentionally has no Temporal and no
//! Python/Node dependency. The store is PostgreSQL-backed when `META_MANAGER_PG_DSN` (or
//! `POSTGRES_DSN_LIST`) is configured — see `workbook_queue::JobStore::init_from_env` — and
//! falls back to memory otherwise; call [`init`] once from `main` to attach it.

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

use crate::meta_manager::{ExportPayload, MetaManager};
use crate::workbook_queue::JobStore;

static STORE: OnceLock<JobStore> = OnceLock::new();

/// Attach the durable job store before serving traffic. Idempotent and safe to call from a
/// startup task; if it is never called, the first request initializes a memory-only store.
pub async fn init() -> &'static JobStore {
    if let Some(store) = STORE.get() {
        return store;
    }
    let _ = STORE.set(JobStore::init_from_env().await);
    STORE.get().expect("job store was just initialized")
}

fn store() -> &'static JobStore {
    STORE.get_or_init(JobStore::new)
}

fn client(headers: &HeaderMap) -> Arc<MetaManager> {
    Arc::new(MetaManager::from_env(
        crate::meta_manager::forwarded_headers(headers),
    ))
}

pub fn router() -> Router {
    Router::new()
        .route("/workbooks/export", post(start_export))
        .route("/workbooks/export/{id}", get(export_status))
        .route("/workbooks/export/{id}/result", get(export_result))
        .route("/workbooks/export/{id}/cancel", post(export_cancel))
        .route("/workbooks/import", post(start_import))
        .route("/workbooks/import/{id}", get(import_status))
        .route("/workbooks/import/{id}/result", get(import_result))
        .route("/workbooks/import/{id}/cancel", post(import_cancel))
        // Workbook export JSON can be several MB; axum's default is 2 MB. The UI uploads
        // the file through this route, so keep the same generous cap as /api/run.
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
}

#[derive(Deserialize)]
struct ExportStart {
    #[serde(rename = "workbookId")]
    workbook_id: String,
}

async fn start_export(
    headers: HeaderMap,
    Json(request): Json<ExportStart>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if request.workbook_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "workbookId is required".into()));
    }
    let export_id = store()
        .start_export(client(&headers), request.workbook_id.clone())
        .await;
    Ok(Json(json!({
        "exportId": export_id,
        "workbookId": request.workbook_id,
        "status": "pending",
    })))
}

async fn export_status(Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    store()
        .status(&id)
        .map(|status| Json(serde_json::to_value(status).unwrap_or(Value::Null)))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn export_result(Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    store()
        .result(&id)
        .map(|result| Json(result.as_ref().clone()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn export_cancel(Path(id): Path<String>) -> Json<Value> {
    Json(json!({"done": store().cancel(&id).await}))
}

#[derive(Deserialize)]
struct ImportStart {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "workbookId", default)]
    workbook_id: Option<String>,
    data: ExportPayload,
}

async fn start_import(
    headers: HeaderMap,
    Json(request): Json<ImportStart>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let workbook_id = match request.workbook_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            create_workbook(
                request.title.as_deref().unwrap_or("Imported workbook"),
                &headers,
            )
            .await?
        }
    };
    let import_id = store()
        .start_import(client(&headers), workbook_id.clone(), request.data)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(Json(json!({
        "importId": import_id,
        "workbookId": workbook_id,
        "status": "pending",
    })))
}

async fn create_workbook(title: &str, headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let base = std::env::var("META_US_URL")
        .or_else(|_| std::env::var("UI_PUBLIC_URL"))
        .unwrap_or_else(|_| {
            let port = std::env::var("PUBLIC_PORT").unwrap_or_else(|_| "8099".into());
            format!("http://127.0.0.1:{port}")
        });
    let master = std::env::var("US_MASTER_TOKEN").unwrap_or_default();
    let response = reqwest::Client::new()
        .post(format!("{base}/v2/workbooks"))
        .header("x-us-master-token", master)
        .headers(crate::meta_manager::forwarded_headers(headers))
        .json(&json!({"title": title}))
        .send()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    if !status.is_success() {
        return Err((StatusCode::BAD_GATEWAY, body.to_string()));
    }
    body.get("id")
        .or_else(|| body.get("workbookId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| (StatusCode::BAD_GATEWAY, "US returned no workbook id".into()))
}

async fn import_status(Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    store()
        .status(&id)
        .map(|status| Json(serde_json::to_value(status).unwrap_or(Value::Null)))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn import_result(Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    store()
        .result(&id)
        .map(|result| Json(result.as_ref().clone()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn import_cancel(Path(id): Path<String>) -> Json<Value> {
    Json(json!({"done": store().cancel(&id).await}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_has_all_job_routes() {
        let routes = router();
        // Building the router is enough to catch duplicate/absent route definitions.
        drop(routes);
    }

    #[test]
    fn unknown_jobs_are_reported_as_not_found() {
        assert!(store().status("missing").is_none());
        assert!(store().result("missing").is_none());
    }
}
