//! The Rust `data-api` service: axum routes over `dl_api::Service`.
//!
//! Routes are the ones `dl_api_lib/app/data_api/app.py` registers for the data path, with
//! the same request/response bodies:
//!
//! ```text
//! GET  /ping
//! GET  /ping_ready
//! GET  /api/data/v2/datasets/{ds_id}/fields
//! POST /api/data/v2/datasets/data/preview
//! POST /api/data/v2/datasets/{ds_id}/preview
//! POST /api/data/v1/datasets/data/preview
//! POST /api/data/v1/datasets/{ds_id}/versions/draft/preview
//! POST /api/data/v2/datasets/{ds_id}/result
//! POST /api/data/v2/datasets/{ds_id}/pivot
//! POST /api/data/v2/datasets/{ds_id}/result-preflight
//! ```
//!
//! The `v1`/`v1.5` preview routes answer the legacy envelope (`ResultResponse::to_v1`); the
//! dataset editor calls them through the gateway's `getPreview`.

pub mod auth;
pub mod codec;
pub mod connections;
pub mod executor;
pub mod metrics;
pub mod state;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codec::{Encoded, Output};
use dl_api::trace::Trace;
use dl_api::{
    ApiError, DashSqlRequest, FieldsItem, FilterItem, OrderByItem, PivotRequest, PivotResponse,
    PreflightResponse, PreviewRequest, ResultRequest, ResultResponse, TypedQueryRawRequest,
    TypedQueryRequest, UpdateAction,
};
use dl_core::{Dataset, FieldRef, FieldType, UserDataType};
use dl_query::{Direction, FilterOperation, Role};
use serde::Deserialize;
use tower_http::compression::CompressionLayer;

pub use codec::{Format, JSON as JSON_CONTENT_TYPE, MSGPACK as MSGPACK_CONTENT_TYPE};
pub use connections::UsConnections;
pub use executor::AnyExecutor;
pub use state::{AppState, DatasetStore};

/// Legacy `?rev_id=` pin of the dataset revision (Python `DatasetViewBase.rev_id`).
#[derive(Debug, Default, Deserialize)]
pub struct LegacyRevQuery {
    #[serde(default)]
    pub rev_id: Option<String>,
}

/// The HTTP API.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .route("/ping_ready", get(ping))
        // `UnistatView` and `DSDataApiMetricsView`: the data-api's own counters. Both are
        // auth-free in Python (`SKIP_AUTH`). The public container entrypoint routes `/metrics`
        // to the `us` service, so this one is exposed by the data-api listener itself.
        .route("/unistat/", get(metrics::unistat))
        .route("/unistat", get(metrics::unistat))
        .route("/metrics", get(metrics::scrape))
        // Cross-request query-result cache counters (task-25 / v9.0).
        .route("/debug/cache", get(debug_cache))
        .route("/api/data/v2/datasets/data/preview", post(preview_inline))
        .route("/api/data/v2/datasets/{ds_id}/pivot", post(pivot))
        .route("/api/data/v2/datasets/{ds_id}/result", post(result))
        .route("/api/data/v2/datasets/{ds_id}/result-raw", post(result_raw))
        .route(
            "/api/data/v2/datasets/{ds_id}/result-preflight",
            post(result_preflight),
        )
        .route(
            "/api/data/v2/datasets/{ds_id}/preview",
            post(preview_stored),
        )
        // `DatasetPreviewViewV1` / `V1_5`: the same query as the `v2` preview, answered in the
        // legacy envelope. `getPreview` of the dataset editor is the `versions/draft` one.
        .route(
            "/api/data/v1/datasets/data/preview",
            post(preview_v1_inline),
        )
        .route("/api/v1/datasets/data/preview", post(preview_v1_inline))
        .route(
            "/api/data/v1.5/datasets/data/preview",
            post(preview_v1_inline),
        )
        .route(
            "/api/data/v1/datasets/{ds_id}/versions/draft/preview",
            post(preview_v1_stored),
        )
        .route(
            "/api/v1/datasets/{ds_id}/versions/draft/preview",
            post(preview_v1_stored),
        )
        .route(
            "/api/data/v1.5/datasets/{ds_id}/preview",
            post(preview_v1_stored),
        )
        // `v1`/`v1.5` data routes (G01/G06/G07): the old chart URL `apiVersion != 2` picks.
        .route(
            "/api/v1/datasets/{ds_id}/versions/draft/result",
            post(result_v1),
        )
        .route(
            "/api/data/v1/datasets/{ds_id}/versions/draft/result",
            post(result_v1),
        )
        .route("/api/data/v1.5/datasets/{ds_id}/result", post(result_v1_5))
        .route(
            "/api/v1/datasets/{ds_id}/versions/draft/values/distinct",
            post(distinct_v1),
        )
        .route(
            "/api/data/v1/datasets/{ds_id}/versions/draft/values/distinct",
            post(distinct_v1),
        )
        .route(
            "/api/data/v1.5/datasets/{ds_id}/values/distinct",
            post(distinct_v1),
        )
        .route(
            "/api/v1/datasets/{ds_id}/versions/draft/values/range",
            post(range_v1),
        )
        .route(
            "/api/data/v1/datasets/{ds_id}/versions/draft/values/range",
            post(range_v1),
        )
        .route(
            "/api/data/v1.5/datasets/{ds_id}/values/range",
            post(range_v1),
        )
        // The legacy pivot answers the v2 pivot body (`DatasetPivotView` is the same view).
        .route("/api/v1/datasets/{ds_id}/versions/draft/pivot", post(pivot))
        .route(
            "/api/data/v1/datasets/{ds_id}/versions/draft/pivot",
            post(pivot),
        )
        // SQL charts and the connection SQL editor (`DashSQLView` and its two siblings).
        .route("/api/v1/connections/{conn_id}/dashsql", post(dashsql))
        .route("/api/data/v1/connections/{conn_id}/dashsql", post(dashsql))
        .route(
            "/api/data/v1/connections/{conn_id}/typed_query",
            post(typed_query),
        )
        .route(
            "/api/data/v1/connections/{conn_id}/typed_query_raw",
            post(typed_query_raw),
        )
        .route(
            "/api/data/v2/datasets/{ds_id}/values/distinct",
            post(distinct),
        )
        .route("/api/data/v2/datasets/{ds_id}/values/range", post(range))
        .route("/api/data/v2/datasets/{ds_id}/fields", get(fields))
        // The charts engine (`getDataSetFieldsById`) asks for `fields` through the
        // legacy `v1` route, so it has to answer there as well.
        .route("/api/data/v1/datasets/{ds_id}/fields", get(fields))
        .route("/api/v1/datasets/{ds_id}/fields", get(fields))
        .layer(compression())
        // The layer sits inside `with_state`: it needs `AppState` to know whether NATIVE auth is
        // configured, and it runs the handler inside the token scope `dl_us` reads.
        .layer(axum::middleware::from_fn(connection_memo))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn(trace_request))
        .layer(axum::middleware::from_fn(snapshot_scope))
        // Outermost, so the 401s of the auth middleware and the compression cost are counted.
        .layer(axum::middleware::from_fn(metrics::middleware))
        .with_state(state)
}

async fn connection_memo(request: Request, next: Next) -> Response {
    dl_api::memo::with_connection_memo(next.run(request)).await
}

async fn snapshot_scope(request: Request, next: Next) -> Response {
    let snapshot = request
        .headers()
        .get("x-dl-snapshot")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    match snapshot {
        Some(snapshot) => dl_us::SNAPSHOT.scope(snapshot, next.run(request)).await,
        None => next.run(request).await,
    }
}

/// One timing line per data request.
///
/// The trace is created outside auth, so the line covers JWT validation, dataset lookup,
/// compilation, database waits, decoding and response assembly. `work` is CPU time in this
/// process; `wait` is time spent in the database/`us`/DSN resolution.
async fn trace_request(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if matches!(
        path.as_str(),
        "/ping" | "/ping_ready" | "/metrics" | "/unistat" | "/unistat/"
    ) {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let trace = std::sync::Arc::new(Trace::new());
    let response =
        dl_api::trace::with_trace(std::sync::Arc::clone(&trace), next.run(request)).await;
    let summary = trace.summary();
    tracing::info!(
        target: "dl_timing",
        method = %method,
        path = %path,
        total_us = summary.total.as_secs_f64() * 1_000_000.0,
        work_us = summary.work.as_secs_f64() * 1_000_000.0,
        wait_us = summary.wait.as_secs_f64() * 1_000_000.0,
        stages = %summary.stages_json(),
        "request timing"
    );
    response
}

/// `Content-Encoding` for the answers.
///
/// A chart body is JSON with repetitive keys and numbers stored as strings: on the demo corpus
/// a compressed answer is a fraction of the original, and a 13 MiB preview becomes ~1.5 MiB.
/// `zstd` is preferred by every current browser and is the cheapest of the three to produce;
/// `br` wins on ratio but costs more CPU per request.
///
/// The predicate is tower-http's default: bodies above 32 bytes, and not `text/event-stream`,
/// gRPC or an image. `Vary: accept-encoding` is added by the layer.
pub fn compression() -> CompressionLayer {
    CompressionLayer::new()
        .zstd(true)
        .br(true)
        .gzip(true)
        .deflate(false)
}

async fn ping() -> &'static str {
    "ok"
}

/// `GET /debug/cache`: bounded query-result cache counters and effective TTL settings.
async fn debug_cache(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.service.query_cache_json())
}

async fn result(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
    ApiJson(mut request): ApiJson<ResultRequest>,
) -> Result<Encoded<ResultResponse>, HttpError> {
    // An inline dataset arrives owned; a stored one is shared, so no request copies it.
    let dataset = match request.dataset.take() {
        Some(dataset) => Arc::new(dataset),
        None => {
            state
                .dataset_rev(&ds_id, request.revision_id.as_deref())
                .await?
        }
    };
    let response = state.service.result(&dataset, &request).await?;
    Ok(format.0.encode(response))
}

async fn preview_inline(
    State(state): State<AppState>,
    format: Output,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Encoded<ResultResponse>, HttpError> {
    let response = state.service.preview(&request).await?;
    Ok(format.0.encode(response))
}

/// `POST /api/data/v2/datasets/{ds_id}/pivot`.
///
/// The body is a `/result` body plus `pivot`; the answer holds `pivot_data` instead of
/// `result_data` (`DatasetPivotView.post`).
async fn pivot(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    format: Output,
    ApiJson(mut request): ApiJson<PivotRequest>,
) -> Result<Encoded<PivotResponse>, HttpError> {
    let dataset = match request.dataset.take() {
        Some(dataset) => Arc::new(dataset),
        None => match legacy.rev_id.as_deref() {
            Some(rev) => state.dataset_rev(&ds_id, Some(rev)).await?,
            None => state.dataset(&ds_id).await?,
        },
    };
    let response = state.service.pivot(&dataset, &request).await?;
    Ok(format.0.encode(response))
}

/// `POST /api/data/v2/datasets/{ds_id}/result-preflight`.
///
/// The status is the one `DatasetResultPreflightView` picks: `200` when the dataset validates,
/// `400` when it does not (and when an `updates` action is refused).
/// Raw JSON result: the same `ResultResponse` bytes, encoded once into a `Bytes` body.
/// Used by the monolith's raw path so engine does not parse a `serde_json::Value`.
async fn result_raw(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    ApiJson(mut request): ApiJson<ResultRequest>,
) -> Result<Response, HttpError> {
    let dataset = match request.dataset.take() {
        Some(dataset) => Arc::new(dataset),
        None => state.dataset(&ds_id).await?,
    };
    let response = state.service.result(&dataset, &request).await?;
    let export = serde_json::to_string(&response.data_export)
        .map_err(|error| HttpError(ApiError::internal(error.to_string())))?;
    let query = response
        .blocks
        .first()
        .and_then(|block| block.query.clone())
        .unwrap_or_default();
    let bytes = serde_json::to_vec(&response)
        .map_err(|error| HttpError(ApiError::internal(error.to_string())))?;
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-dl-data-export", export);
    if let Ok(value) = axum::http::HeaderValue::from_str(&query) {
        builder = builder.header("x-dl-query", value);
    }
    builder
        .body(Body::from(bytes))
        .map_err(|error| HttpError(ApiError::internal(error.to_string())))
}

async fn result_preflight(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
    ApiJson(mut request): ApiJson<ResultRequest>,
) -> Result<(StatusCode, Encoded<PreflightResponse>), HttpError> {
    let dataset = match request.dataset.take() {
        Some(dataset) => Arc::new(dataset),
        None => state.dataset(&ds_id).await?,
    };
    let preflight = state.service.preflight(&dataset, &request);
    let status =
        StatusCode::from_u16(preflight.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    Ok((status, format.0.encode(preflight.body)))
}

async fn preview_stored(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Encoded<ResultResponse>, HttpError> {
    // The dataset stays an `Arc` from the store/cache: no copy of it is made to answer, and the
    // service takes it by reference rather than through the request body.
    let stored = match request.dataset.is_some() {
        true => None,
        false => Some(state.dataset(&ds_id).await?),
    };
    let response = state
        .service
        .preview_with(stored.as_deref(), &request)
        .await?;
    Ok(format.0.encode(response))
}

async fn preview_v1_inline(
    State(state): State<AppState>,
    format: Output,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Encoded<serde_json::Value>, HttpError> {
    let response = state.service.preview_v1(&request).await?;
    Ok(format.0.encode(response))
}

/// `POST /api/data/v1/datasets/{ds_id}/versions/draft/preview`, the gateway's `getPreview`.
///
/// The body may carry the dataset inline (the editor sends the whole `result_schema`, its
/// `sources` and the unsaved fields), and the stored dataset answers when it does not
/// (`DatasetPreviewView.STORED_DATASET_REQUIRED = False`).
async fn preview_v1_stored(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    format: Output,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Encoded<serde_json::Value>, HttpError> {
    let stored = match request.dataset.is_some() {
        true => None,
        false => Some(match legacy.rev_id.as_deref() {
            Some(rev) => state.dataset_rev(&ds_id, Some(rev)).await?,
            None => state.dataset(&ds_id).await?,
        }),
    };
    let response = state
        .service
        .preview_v1_with(stored.as_deref(), &request)
        .await?;
    Ok(format.0.encode(response))
}

async fn fields(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
) -> Result<Encoded<serde_json::Value>, HttpError> {
    let (dataset, revision) = state.dataset_with_revision(&ds_id).await?;
    let started = std::time::Instant::now();
    let fields = dataset_fields(&dataset, revision.as_deref());
    dl_api::trace::record_work("fields", started.elapsed());
    Ok(format.0.encode(fields))
}

async fn distinct(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
    ApiJson(request): ApiJson<dl_api::DistinctRequest>,
) -> Result<Encoded<ResultResponse>, HttpError> {
    let dataset = state.dataset(&ds_id).await?;
    let response = state.service.distinct(&dataset, &request).await?;
    Ok(format.0.encode(response))
}

async fn range(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    format: Output,
    ApiJson(request): ApiJson<dl_api::RangeRequest>,
) -> Result<Encoded<ResultResponse>, HttpError> {
    let dataset = state.dataset(&ds_id).await?;
    let response = state.service.range(&dataset, &request).await?;
    Ok(format.0.encode(response))
}

/// `DatasetVersionResultRequestSchema`: the pre-v2 `/result` body (`columns`, `where`, ...).
#[derive(Debug, Deserialize)]
struct LegacyResultRequest {
    columns: Vec<String>,
    #[serde(rename = "where", default)]
    filters: Vec<LegacyWhere>,
    #[serde(default)]
    group_by: Vec<String>,
    #[serde(default)]
    order_by: Vec<LegacyOrderBy>,
    #[serde(default, deserialize_with = "dl_api::dto::lenient_int::optional")]
    limit: Option<i64>,
    #[serde(default, deserialize_with = "dl_api::dto::lenient_int::optional")]
    offset: Option<i64>,
    #[serde(default)]
    updates: Vec<UpdateAction>,
    #[serde(default, deserialize_with = "dl_api::dto::lenient_int::optional")]
    row_count_hard_limit: Option<i64>,
    #[serde(default)]
    with_totals: bool,
    #[serde(default)]
    disable_group_by: bool,
    #[serde(default = "default_true")]
    add_fields_data: bool,
    #[serde(default)]
    ignore_nonexistent_filters: bool,
    #[serde(default)]
    revision_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyWhere {
    column: String,
    operation: FilterOperation,
    #[serde(default)]
    values: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct LegacyOrderBy {
    column: String,
    #[serde(default)]
    direction: Direction,
}

fn default_true() -> bool {
    true
}

/// `IdOrTitleFieldRef`: a v1 string names a field id first, its title second.
fn legacy_field_ref(dataset: &Dataset, name: &str) -> FieldRef {
    match dataset.field_by_guid(name).is_some() {
        true => FieldRef::id(name),
        false => FieldRef::title(name),
    }
}

impl LegacyResultRequest {
    /// Translate the v1 body into the v2 [`ResultRequest`] the service compiles.
    ///
    /// A v1 `columns` entry becomes a `row` item for a dimension and a `measure` item for a
    /// measure, which is the role the old frontend meant by listing it; `group_by` is implied by
    /// the row items, and `where` is the same filter list under another name.
    fn into_result_request(self, dataset: &Dataset) -> ResultRequest {
        let mut fields: Vec<FieldsItem> = self
            .columns
            .iter()
            .map(|name| {
                let field_ref = legacy_field_ref(dataset, name);
                let field_type = match &field_ref {
                    FieldRef::Id { id } => dataset.field_by_guid(id).map(|f| dataset.field_type(f)),
                    FieldRef::Title { title } => {
                        dataset.field_by_title(title).map(|f| dataset.field_type(f))
                    }
                    _ => None,
                };
                let role = match field_type {
                    Some(FieldType::Measure) => Role::Measure,
                    _ => Role::Row,
                };
                FieldsItem::new(field_ref, role)
            })
            .collect();
        // `LegendFormalizer.generate_legend_items`: a `group_by` field that is not in `columns`
        // is still part of the legend, as a `row` item. A field already selected keeps its role.
        for name in &self.group_by {
            if !self.columns.contains(name) {
                fields.push(FieldsItem::new(legacy_field_ref(dataset, name), Role::Row));
            }
        }
        let filters = self
            .filters
            .iter()
            .map(|filter| {
                FilterItem::new(
                    legacy_field_ref(dataset, &filter.column),
                    filter.operation,
                    filter.values.clone(),
                )
            })
            .collect();
        let order_by = self
            .order_by
            .iter()
            .map(|item| OrderByItem::new(legacy_field_ref(dataset, &item.column), item.direction))
            .collect();
        ResultRequest {
            fields,
            order_by,
            filters,
            limit: self.limit,
            offset: self.offset,
            disable_group_by: self.disable_group_by,
            row_count_hard_limit: self.row_count_hard_limit,
            updates: self.updates,
            ignore_nonexistent_filters: self.ignore_nonexistent_filters,
            revision_id: self.revision_id,
            result: self
                .with_totals
                .then_some(dl_api::ResultSpec { with_totals: true }),
            ..ResultRequest::default()
        }
    }
}

/// `ResultDataRequestV1_5Schema`: a v2 body plus the two top-level v1.5 flags. The v2 body keeps
/// rejecting/unignoring them, so the flags are read here and folded into the v2 request.
#[derive(Debug, Deserialize)]
struct ResultRequestV15 {
    #[serde(flatten)]
    request: ResultRequest,
    #[serde(default = "default_true")]
    add_fields_data: bool,
    #[serde(default)]
    with_totals: bool,
}

impl ResultRequestV15 {
    fn into_result_request(self) -> (ResultRequest, bool) {
        let mut request = self.request;
        if self.with_totals {
            request.result = Some(dl_api::ResultSpec { with_totals: true });
        }
        (request, self.add_fields_data)
    }
}

/// `POST /api/v1|data/v1/datasets/{ds_id}/versions/draft/result` (`DatasetResultViewV1`).
async fn result_v1(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    ApiJson(request): ApiJson<LegacyResultRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let dataset = match legacy.rev_id.as_deref() {
        Some(rev) => state.dataset_rev(&ds_id, Some(rev)).await?,
        None => state.dataset(&ds_id).await?,
    };
    let add_fields_data = request.add_fields_data;
    let request = request.into_result_request(&dataset);
    let response = state.service.result(&dataset, &request).await?;
    Ok(Json(response.to_v1_result(&dataset, add_fields_data)))
}

/// `POST /api/data/v1.5/datasets/{ds_id}/result` (`DatasetResultViewV1_5`): a v2 body, the v1
/// envelope.
async fn result_v1_5(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    ApiJson(request): ApiJson<ResultRequestV15>,
) -> Result<Json<serde_json::Value>, HttpError> {
    // `ResultDataRequestV1_5Schema` has no inline `dataset`; a stored dataset answers.
    let (mut request, add_fields_data) = request.into_result_request();
    let dataset = match request.dataset.take() {
        Some(dataset) => Arc::new(dataset),
        None => {
            let revision = legacy.rev_id.as_deref().or(request.revision_id.as_deref());
            state.dataset_rev(&ds_id, revision).await?
        }
    };
    let response = state.service.result(&dataset, &request).await?;
    Ok(Json(response.to_v1_result(&dataset, add_fields_data)))
}

/// `POST .../values/distinct` in the v1/v1.5 envelope (`DatasetDistinctViewV1`/`V1_5`).
async fn distinct_v1(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    ApiJson(request): ApiJson<dl_api::DistinctRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let dataset = match legacy.rev_id.as_deref() {
        Some(rev) => state.dataset_rev(&ds_id, Some(rev)).await?,
        None => state.dataset(&ds_id).await?,
    };
    let response = state.service.distinct(&dataset, &request).await?;
    Ok(Json(response.to_v1()))
}

/// `POST .../values/range` in the v1/v1.5 envelope: two rows of one column.
async fn range_v1(
    State(state): State<AppState>,
    Path(ds_id): Path<String>,
    Query(legacy): Query<LegacyRevQuery>,
    ApiJson(request): ApiJson<dl_api::RangeRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let dataset = match legacy.rev_id.as_deref() {
        Some(rev) => state.dataset_rev(&ds_id, Some(rev)).await?,
        None => state.dataset(&ds_id).await?,
    };
    let response = state.service.range(&dataset, &request).await?;
    Ok(Json(response.to_v1_range()))
}

/// `POST /api/data/v1/connections/{conn_id}/dashsql` (`DashSQLView`).
async fn dashsql(
    State(state): State<AppState>,
    Path(conn_id): Path<String>,
    ApiJson(request): ApiJson<DashSqlRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    Ok(Json(state.service.dashsql(&conn_id, &request).await?))
}

/// `POST /api/data/v1/connections/{conn_id}/typed_query` (`DashSQLTypedQueryView`).
async fn typed_query(
    State(state): State<AppState>,
    Path(conn_id): Path<String>,
    ApiJson(request): ApiJson<TypedQueryRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    Ok(Json(state.service.typed_query(&conn_id, &request).await?))
}

/// `POST /api/data/v1/connections/{conn_id}/typed_query_raw` (`DashSQLTypedQueryRawView`).
async fn typed_query_raw(
    State(state): State<AppState>,
    Path(conn_id): Path<String>,
    ApiJson(request): ApiJson<TypedQueryRawRequest>,
) -> Result<Json<serde_json::Value>, HttpError> {
    Ok(Json(
        state.service.typed_query_raw(&conn_id, &request).await?,
    ))
}

fn field_dtype(field: &dl_core::Field) -> UserDataType {
    field
        .data_type
        .or(field.cast)
        .or(field.initial_data_type)
        .unwrap_or(UserDataType::String)
}

/// `DatasetFieldsResponseSchema`: the dataset as stored, not a legend.
///
/// Every key is the one `FieldsResponseFieldSchema` declares; `default_value` is sent for
/// parameters so a chart can fall back to it.
fn dataset_fields(dataset: &Dataset, revision: Option<&str>) -> serde_json::Value {
    let fields: Vec<serde_json::Value> = dataset
        .fields
        .iter()
        .map(|field| {
            let dtype = field_dtype(field);
            serde_json::json!({
                "title": field.title,
                "guid": field.guid,
                "data_type": dtype,
                "cast": dtype,
                "hidden": field.hidden,
                "type": dataset.field_type(field),
                "calc_mode": field.calc_mode,
                "ui_settings": field.ui_settings,
                "description": field.description,
                "formula": field.formula,
                "aggregation": field.aggregation,
                "default_value": field.default_value,
            })
        })
        .collect();
    // `getDatasetFields` reads `fields` and `revision_id`; the stored dataset has no
    // revision of its own here, so the revision is reported as unknown.
    serde_json::json!({ "fields": fields, "revision_id": revision })
}

/// A JSON body extractor that answers the `data-api` error envelope, not axum's `422` text.
///
/// `Json<T>` rejects a body it cannot deserialize with `422` and a plain-text message, which the
/// UI cannot show and which no Python client expects: `data-api` answers `400` with
/// `{"code": "ERR.DS_API.REQUEST", "message": ...}`. The message keeps axum's precise reason
/// (`fields[2].ref.type: unknown variant ...`) so the cause is still visible.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = HttpError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(HttpError(ApiError::bad_request(
                "ERR.DS_API.REQUEST",
                rejection.body_text(),
            ))),
        }
    }
}

/// `data-api` errors: `{"code": ..., "message": ...}` with a matching status code.
pub struct HttpError(pub ApiError);

impl From<ApiError> for HttpError {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // Errors are always JSON: a client that asked for MessagePack still has to be able to
        // read why the request failed, and `{"code", "message", "details", "debug"}` is the
        // shape `RegularAPIErrorSchema` sends.
        let body = serde_json::json!({
            "code": self.0.code,
            "message": self.0.message,
            "details": self.0.details,
            "debug": self.0.debug,
        });
        (
            status,
            [(header::CONTENT_TYPE, codec::JSON)],
            body.to_string(),
        )
            .into_response()
    }
}

/// Build a state that never touches a database: SQL is compiled and returned, rows are empty.
pub fn dry_run_state(dialect: dl_query::Dialect) -> AppState {
    AppState::new(
        Arc::new(dl_api::Service::new(AnyExecutor::DryRun, dialect)),
        Arc::new(DatasetStore::with_defaults()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::Serialize;
    use tower::ServiceExt;

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    const TEST_PRIVATE_KEY: &str = include_str!("../tests/fixtures/test-private.pem");
    const TEST_PUBLIC_KEY: &str = include_str!("../tests/fixtures/test-public.pem");

    #[derive(Serialize)]
    struct TestClaims {
        #[serde(rename = "userId")]
        user_id: String,
        exp: usize,
        roles: Vec<String>,
    }

    fn test_auth() -> auth::NativeAuth {
        auth::NativeAuth::from_pem(TEST_PUBLIC_KEY.as_bytes(), "PS256").unwrap()
    }

    fn test_token() -> String {
        let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).unwrap();
        let claims = TestClaims {
            user_id: "user-1".to_string(),
            exp: 4_102_444_800,
            roles: vec!["viewer".to_string()],
        };
        encode(&Header::new(Algorithm::PS256), &claims, &key).unwrap()
    }

    fn native_dry_run() -> Router {
        router(dry_run_state(dl_query::Dialect::Postgres).with_auth(test_auth()))
    }

    #[tokio::test]
    async fn native_auth_rejects_a_missing_token() {
        let response = native_dry_run()
            .oneshot(
                Request::builder()
                    .uri("/api/data/v2/datasets/demo/fields")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.REQUEST.AUTH");
        // `RegularAPIErrorSchema`: the envelope always carries `details` and `debug`, even when
        // they are empty.
        assert_eq!(body["details"], serde_json::json!({}));
        assert_eq!(body["debug"], serde_json::json!({}));
    }

    #[tokio::test]
    async fn native_auth_accepts_a_valid_token() {
        let response = native_dry_run()
            .oneshot(
                Request::builder()
                    .uri("/api/data/v2/datasets/demo/fields")
                    .header(header::AUTHORIZATION, format!("Bearer {}", test_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ping_stays_public_with_native_auth() {
        let response = native_dry_run()
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_native_request_reads_us_with_the_callers_token() {
        use dl_us::testing::MockUs;
        use dl_us::{UsClient, UsDatasetCache};
        use std::time::Duration;

        let us = MockUs::with_dataset().await;
        let cache = std::sync::Arc::new(UsDatasetCache::new(
            UsClient::new(us.base_url())
                .unwrap()
                .with_master_token("service-token"),
            Duration::from_secs(30),
        ));
        let state = dry_run_state(dl_query::Dialect::Postgres)
            .with_us(cache)
            .with_auth(test_auth());
        let token = test_token();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/data/v2/datasets/dataset-1/fields")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let requests = us.requests();
        assert_eq!(requests[0].master_token, None);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some(format!("Bearer {token}").as_str())
        );
    }

    #[tokio::test]
    async fn rls_filters_the_result_for_the_authenticated_user() {
        use dl_core::{Rls, RlsEntry, RlsPattern, RlsSubject, RlsSubjectType};

        let mut dataset = dl_api::testing::demo_dataset();
        dataset.rls = Rls {
            entries: vec![RlsEntry {
                field_guid: "f_region".to_string(),
                allowed_value: Some("Europe".to_string()),
                pattern_type: RlsPattern::Value,
                subject: RlsSubject {
                    subject_type: RlsSubjectType::User,
                    subject_id: "user-1".to_string(),
                    subject_name: Some("user-1".to_string()),
                },
            }],
        };
        let mut store = DatasetStore::with_defaults();
        store.insert("demo", dataset);
        let state = AppState::new(
            Arc::new(dl_api::Service::new(
                AnyExecutor::DryRun,
                dl_query::Dialect::Postgres,
            )),
            Arc::new(store),
        )
        .with_auth(test_auth());

        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
            ],
        });
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {}", test_token()))
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert!(sql.contains(r#"t0."Region" IN ('Europe')"#), "{sql}");
    }

    #[tokio::test]
    async fn ping_answers_ok() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let response = app
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn preview_returns_sql_and_the_legend_over_http() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({ "dataset": dl_api::testing::demo_dataset() });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/data/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["fields"][0]["id"], "f_region");
        assert_eq!(body["fields"][1]["data_type"], "integer");
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert!(sql.starts_with("SELECT t0.\"Region\""), "{sql}");
        assert!(sql.contains("FROM \"public\".\"sales\" AS t0"));
        assert_eq!(body["result_data"][0]["rows"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_v1_draft_preview_answers_the_legacy_envelope() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        // `getPreview`: the editor posts the dataset inline and reads `result.data`.
        let payload = serde_json::json!({
            "datasetId": "demo",
            "limit": 10,
            "dataset": dl_api::testing::demo_dataset(),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v1/datasets/demo/versions/draft/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["result"]["data"]["Data"], serde_json::json!([]));
        assert_eq!(body["result"]["data"]["Type"][0], "ListType");
        assert_eq!(body["result"]["data"]["Type"][1][0], "StructType");
        assert_eq!(
            body["result"]["data"]["Type"][1][1][0],
            serde_json::json!(["Region", ["OptionalType", ["DataType", "String"]]])
        );
        // The `v2` envelope is gone: the client has no use for `result_data` here.
        assert!(body.get("result_data").is_none());
    }

    #[tokio::test]
    async fn a_stringly_typed_preview_limit_is_accepted() {
        // The editor's "Number of rows" field posts `limit` as a JSON *string* (`"100"`):
        // pydantic coerces it, so the port has to read it the same way.
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "datasetId": "demo",
            "limit": "100",
            "dataset": dl_api::testing::demo_dataset(),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v1/datasets/demo/versions/draft/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    /// A `?` on the demo dataset, asked for twice: once as JSON, once as MessagePack.
    async fn preview_answers(format: Option<&str>) -> (StatusCode, String, Vec<u8>) {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({ "dataset": dl_api::testing::demo_dataset() });
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/data/v2/datasets/data/preview")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(accept) = format {
            builder = builder.header(header::ACCEPT, accept);
        }
        let response = app
            .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, content_type, bytes.to_vec())
    }

    #[tokio::test]
    async fn a_msgpack_answer_carries_the_same_body_as_json() {
        let (json_status, json_type, json_bytes) = preview_answers(None).await;
        assert_eq!(json_status, StatusCode::OK);
        assert_eq!(json_type, "application/json");

        let (packed_status, packed_type, packed_bytes) =
            preview_answers(Some("application/msgpack")).await;
        assert_eq!(packed_status, StatusCode::OK);
        assert_eq!(packed_type, "application/msgpack");
        assert_ne!(packed_bytes.first(), Some(&b'{'));

        let decoded: serde_json::Value = rmp_serde::from_slice(&packed_bytes).unwrap();
        let expected: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn a_gzip_client_gets_the_body_compressed() {
        use std::io::Read;

        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({ "dataset": dl_api::testing::demo_dataset() });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/data/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
        // The body still says which headers it depends on: ours (`accept`) and the layer's
        // (`accept-encoding`) are separate values of the same `Vary`.
        let vary: String = response
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            vary.to_ascii_lowercase().contains("accept-encoding"),
            "vary: {vary}"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_end(&mut decoded)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert!(value["fields"].is_array());
    }

    #[tokio::test]
    async fn result_for_an_unknown_dataset_is_a_404_with_a_code() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/nope/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.DB.DATASET_NOT_FOUND");
    }

    #[tokio::test]
    async fn result_for_the_demo_dataset_compiles_against_the_store() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}}
            ]
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert!(sql.contains("SUM(t0.\"Sales\") AS \"f_sales\""), "{sql}");
    }

    #[tokio::test]
    async fn pivot_is_served_over_http() {
        let state = AppState::new(
            Arc::new(dl_api::Service::new(
                AnyExecutor::Static(Arc::new(vec![
                    vec![Some("Europe".to_string()), Some("42".to_string())],
                    vec![Some("Asia".to_string()), Some("7".to_string())],
                ])),
                dl_query::Dialect::Postgres,
            )),
            Arc::new(DatasetStore::with_defaults()),
        );
        let app = router(state);
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}}
            ],
            "pivot": {
                "structure": [
                    {"legend_item_ids": [0], "role_spec": {"role": "pivot_row"}},
                    {"legend_item_ids": [1], "role_spec": {"role": "pivot_measure"}}
                ]
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/pivot")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;

        // `pivot.structure` is the request order plus the patched pseudo-dimensions.
        assert_eq!(body["pivot"]["structure"][0]["title"], "Region");
        assert_eq!(body["pivot"]["structure"][1]["title"], "Sales");
        assert_eq!(body["pivot"]["structure"][2]["title"], "Measure Names");
        assert_eq!(body["pivot"]["structure"][3]["title"], "Dimension Names");

        // One column (`Measure Names` -> `Sales`) and rows sorted by `Region`.
        assert_eq!(body["pivot_data"]["columns"][0][0][0][0], "Sales");
        assert_eq!(
            body["pivot_data"]["row_dimension_headers"][0][0][0],
            "Region"
        );
        assert_eq!(body["pivot_data"]["rows"][0]["header"][0][0][0], "Asia");
        assert_eq!(body["pivot_data"]["rows"][0]["values"][0][0][0], "7");
        assert_eq!(body["pivot_data"]["rows"][1]["header"][0][0][0], "Europe");
        assert_eq!(body["pivot_data"]["rows"][1]["values"][0][0][0], "42");
        // A cell is `[value, legend_item_id, pivot_item_id]`.
        assert_eq!(body["pivot_data"]["rows"][1]["values"][0][0][1], 1);
        assert_eq!(body["pivot_data"]["rows"][1]["values"][0][0][2], 1);

        assert_eq!(body["data_export"]["background"]["allowed"], false);
        // The tenant ban, then the pivot one (`get_data_export_base_result` + the pivot patch).
        assert_eq!(
            body["data_export"]["background"]["reason"][0],
            "PROHIBITED_EXPORT_TENANT"
        );
        assert_eq!(
            body["data_export"]["background"]["reason"][1],
            "PROHIBITED_EXPORT_PIVOT_TABLE"
        );
        assert_eq!(body["data_export"]["basic"]["allowed"], true);
    }

    #[tokio::test]
    async fn pivot_totals_add_a_second_block_over_http() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}}
            ],
            "pivot": {
                "structure": [
                    {"legend_item_ids": [0], "role_spec": {"role": "pivot_row"}},
                    {"legend_item_ids": [1], "role_spec": {"role": "pivot_measure"}}
                ],
                "with_totals": true
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/pivot")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;

        // `merged_stream.meta.blocks`: the request's own block plus the row total.
        let blocks = body["blocks"].as_array().expect("the block list");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["block_id"], 0);
        assert_eq!(blocks[1]["block_id"], 1);
        assert!(blocks[0]["query"]
            .as_str()
            .unwrap()
            .contains("GROUP BY t0.\"Region\""));
        assert!(!blocks[1]["query"].as_str().unwrap().contains("GROUP BY"));

        // The total re-uses the measure with the `total` role and fills the dimension with the
        // `template` placeholder; both are part of the response `fields`.
        let fields = body["fields"].as_array().expect("the legend");
        assert!(fields
            .iter()
            .any(|item| item["role_spec"]["role"] == "total" && item["id"] == "f_sales"));
        assert!(fields
            .iter()
            .any(|item| item["role_spec"]["role"] == "template"));
    }

    #[tokio::test]
    async fn rows_come_back_as_data_and_legend_pairs() {
        let state = AppState::new(
            Arc::new(dl_api::Service::new(
                AnyExecutor::Static(Arc::new(vec![vec![
                    Some("Europe".to_string()),
                    Some("42".to_string()),
                ]])),
                dl_query::Dialect::Postgres,
            )),
            Arc::new(DatasetStore::with_defaults()),
        );
        let app = router(state);
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}}
            ]
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        // `make_data_response_v2`: `{"data": [...], "legend": [...]}`, not a bare array, and a
        // measure is a *string* (`postprocess_data` -> `stringify_or_null`).
        assert_eq!(body["result_data"][0]["rows"][0]["data"][0], "Europe");
        assert_eq!(body["result_data"][0]["rows"][0]["data"][1], "42");
        assert_eq!(body["result_data"][0]["rows"][0]["legend"][0], 0);
        assert_eq!(body["result_data"][0]["rows"][0]["legend"][1], 1);
        assert!(!body["data_export_forbidden"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn role_spec_and_autofill_legend_are_served_over_http() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
            "autofill_legend": true
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        // Every field of the dataset is in `fields`, the selected one as `row`, the rest `info`.
        let roles: Vec<(String, String)> = body["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["id"].as_str().unwrap().to_string(),
                    item["role_spec"]["role"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(roles[0], ("f_region".to_string(), "row".to_string()));
        assert_eq!(roles.len(), 4);
        assert!(roles[1..].iter().all(|(_, role)| role == "info"));
        assert_eq!(body["fields"][0]["role_spec"]["visibility"], "visible");
    }

    #[tokio::test]
    async fn a_tree_role_needs_a_tree_str_field() {
        // `DATA_TYPES_SUPPORTING_TREE = {tree_str}`: the demo's `f_region` is a string, so the
        // role is rejected the way `RoleDataTypeMismatchError` does it.
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"},
                        "role_spec": {"role": "tree", "level": 1, "prefix": "[]"}}],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.LEGEND.ROLE_DATA_TYPE_MISMATCH");
        assert_eq!(body["message"], "Unsupported data type for tree role");
    }

    #[tokio::test]
    async fn a_total_role_is_an_ordinary_selectable_item() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [{"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "total"}}],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["fields"][0]["role_spec"]["role"], "total");
        assert_eq!(
            body["blocks"][0]["query"],
            "SELECT SUM(t0.\"Sales\") AS \"f_sales\" FROM \"public\".\"sales\" AS t0 LIMIT 1000001"
        );
    }

    #[tokio::test]
    async fn duplicate_legend_ids_are_a_400_with_the_python_code() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "legend_item_id": 3},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}, "legend_item_id": 3},
            ],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.LEGEND.NON_UNIQUE_IDS");
    }

    #[tokio::test]
    async fn preview_accepts_an_inline_dataset_in_the_us_shape() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        // `DatasetContentInternalSchema` calls the field list `result_schema`; the port accepts
        // it (and `fields`) so a request captured from the Python service can be replayed.
        let payload = serde_json::json!({
            "dataset": {
                "result_schema": [
                    {"guid": "f_region", "title": "Region", "source": "Region",
                     "calc_mode": "direct", "data_type": "string", "type": "DIMENSION"},
                    {"guid": "f_sales", "title": "Sales", "source": "Sales",
                     "calc_mode": "direct", "data_type": "integer", "type": "MEASURE",
                     "aggregation": "sum"}
                ],
                "sources": [{"id": "src", "table": "public.sales"}],
                "data_export_forbidden": true
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/data/preview")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        // `GroupByPolicy.if_measures`: the selection has a measure, so the preview groups by its
        // dimension and applies the field's own aggregation — `DatasetPreviewRequestSchema`.
        assert_eq!(
            body["blocks"][0]["query"],
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" LIMIT 100"
        );
        // The dataset forbids export, and the preview response says so.
        assert!(body["data_export_forbidden"].as_bool().unwrap());
        assert_eq!(body["data_export"]["basic"]["allowed"], false);
        assert_eq!(
            body["data_export"]["basic"]["reason"][0],
            "DISABLED_EXPORT_DATASET"
        );
    }

    #[tokio::test]
    async fn a_refused_update_action_is_a_400_with_a_code() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
            // `clone_field` exists in the dataset schema but the data-api does not accept it.
            "updates": [{"action": "clone_field", "field": {"guid": "f_x", "from_guid": "f_region"}}],
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.ACTION_NOT_ALLOWED");
    }

    #[tokio::test]
    async fn result_preflight_says_ok_for_the_stored_dataset() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result-preflight")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["code"], "OK");
        assert_eq!(body["message"], "Validation was successful");
        assert_eq!(
            body["dataset"]["component_errors"]["items"],
            serde_json::json!([])
        );
        // `get_fields_data_raw` narrowed to the five keys of the preflight schema.
        let field = &body["dataset"]["fields"][0];
        assert_eq!(field["guid"], "f_region");
        assert_eq!(field["title"], "Region");
        assert_eq!(field["data_type"], "string");
        assert_eq!(field["calc_mode"], "direct");
        assert_eq!(field["type"], "DIMENSION");
        let keys: Vec<&String> = field.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 5);
    }

    #[tokio::test]
    async fn result_preflight_reports_a_broken_inline_dataset() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "dataset": {
                "sources": [{"id": "src", "table": "public.sales"}],
                "result_schema": [
                    {"guid": "f_region", "title": "Region", "source": "Region", "data_type": "string"},
                    {
                        "guid": "f_broken",
                        "title": "Broken",
                        "type": "DIMENSION",
                        "calc_mode": "formula",
                        "formula": "SUM(",
                        "data_type": "string"
                    }
                ]
            },
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result-preflight")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["code"], "ERR.DS_API.VALIDATION.ERROR");
        assert_eq!(body["message"], "Validation finished with errors.");
        let pack = &body["dataset"]["component_errors"]["items"][0];
        assert_eq!(pack["id"], "f_broken");
        assert_eq!(pack["type"], "field");
        assert_eq!(pack["errors"][0]["level"], "error");
        assert_eq!(pack["errors"][0]["code"], "ERR.DS_API.FORMULA.VALIDATION");
    }

    #[tokio::test]
    async fn with_totals_is_accepted_and_stays_off_the_v2_body() {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let payload = serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
            ],
            "result": {"with_totals": true},
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/demo/result")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        // The response describes the main block only: `DataApiV2ResponseSchema` has no
        // `totals`, so the second query exists for v1 clients (and for parity with Python).
        assert!(body.get("totals").is_none());
        assert_eq!(body["fields"].as_array().unwrap().len(), 2);
        assert_eq!(body["blocks"].as_array().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod value_endpoint_tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn post_json(uri: &str, payload: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let app = router(dry_run_state(dl_query::Dialect::Postgres));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn distinct_returns_values_of_the_field() {
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/distinct",
            serde_json::json!({"field_guid": "f_region", "limit": 1000}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             ORDER BY \"f_region\" ASC NULLS FIRST LIMIT 1000"
        );
        assert_eq!(body["fields"][0]["id"], "f_region");
        // `/values/distinct` is its own legend formalizer: one item, role `distinct`.
        assert_eq!(body["fields"][0]["role_spec"]["role"], "distinct");
        assert_eq!(body["fields"][0]["legend_item_id"], 0);
    }

    #[tokio::test]
    async fn range_returns_min_and_max() {
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/range",
            serde_json::json!({"field_guid": "f_sales"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert_eq!(
            sql,
            "SELECT MIN(t0.\"Sales\") AS \"f_sales_min\", MAX(t0.\"Sales\") AS \"f_sales_max\" \
             FROM \"public\".\"sales\" AS t0"
        );
        assert_eq!(body["fields"][0]["id"], "f_sales");
        assert_eq!(body["fields"][1]["id"], "f_sales");
        assert_eq!(body["fields"][0]["title"], "Sales");
        // `RangeRoleSpec.range_type`: the two items differ only in which end they ask for.
        assert_eq!(body["fields"][0]["role_spec"]["range_type"], "min");
        assert_eq!(body["fields"][1]["role_spec"]["range_type"], "max");
    }

    #[tokio::test]
    async fn the_v2_distinct_body_names_the_field_in_fields() {
        // The charts engine sends `DistinctDataRequestV2Schema`: the field arrives in `fields`
        // with the `distinct` role, and the filters arrive under `filters`.
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/distinct",
            serde_json::json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "distinct"}}
                ],
                "filters": [
                    {
                        "ref": {"type": "id", "id": "f_region"},
                        "operation": "eq",
                        "values": ["North"]
                    }
                ],
                "limit": 1000,
                "ignore_nonexistent_filters": true,
                "parameter_values": [],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             WHERE t0.\"Region\" = 'North' ORDER BY \"f_region\" ASC NULLS FIRST LIMIT 1000"
        );
        assert_eq!(body["fields"][0]["id"], "f_region");
        assert_eq!(body["fields"][0]["role_spec"]["role"], "distinct");
    }

    #[tokio::test]
    async fn the_v2_range_body_names_the_field_in_fields() {
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/range",
            serde_json::json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "range"}}
                ],
                "filters": [],
                "ignore_nonexistent_filters": false,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert_eq!(
            sql,
            "SELECT MIN(t0.\"Sales\") AS \"f_sales_min\", MAX(t0.\"Sales\") AS \"f_sales_max\" \
             FROM \"public\".\"sales\" AS t0"
        );
    }

    #[tokio::test]
    async fn a_values_request_without_a_field_is_a_400() {
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/distinct",
            serde_json::json!({"filters": [], "limit": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "ERR.DS_API.REQUEST");
    }

    #[tokio::test]
    async fn a_missing_field_guid_is_a_400_for_distinct() {
        let (status, body) = post_json(
            "/api/data/v2/datasets/demo/values/distinct",
            serde_json::json!({"field_guid": "nope"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "ERR.DS_API.FIELD.NOT_FOUND");
    }
}

#[cfg(test)]
mod us_tests {
    use super::*;
    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode};
    use dl_us::testing::MockUs;
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn us_state(us: &MockUs) -> AppState {
        let client = dl_us::UsClient::new(us.base_url())
            .unwrap()
            .with_master_token("AC1ofiek8coB");
        let cache = Arc::new(dl_us::UsDatasetCache::new(client, Duration::from_secs(60)));
        // The demo store is kept: it must not shadow the ids that only `us` knows.
        AppState::new(
            Arc::new(dl_api::Service::new(
                AnyExecutor::DryRun,
                dl_query::Dialect::Postgres,
            )),
            Arc::new(DatasetStore::with_defaults()),
        )
        .with_us(cache)
    }

    async fn get_json(state: AppState, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn a_dataset_id_is_resolved_through_us() {
        let us = MockUs::with_dataset().await;
        let (status, body) = get_json(
            us_state(&us).await,
            "/api/data/v2/datasets/dataset-1/fields",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let region = &body["fields"][0];
        assert_eq!(region["guid"], "dim_region");
        assert_eq!(region["title"], "Region");
        assert_eq!(region["type"], "DIMENSION");
        assert_eq!(region["cast"], "string");
        assert_eq!(body["fields"][1]["type"], "MEASURE");
        assert_eq!(body["fields"][1]["aggregation"], "sum");
        assert_eq!(body["fields"][2]["formula"], "[Sales] * 2");
        // A parameter keeps its calc mode and its default value.
        assert_eq!(body["fields"][3]["calc_mode"], "parameter");
        assert_eq!(body["fields"][3]["default_value"], 0.5);
    }

    /// The charts engine asks for the dataset fields through `/api/data/v1/.../fields`
    /// (`getDataSetFieldsById`); it must answer exactly like the v2 route, `revision_id`
    /// included, or every request becomes a `427`.
    #[tokio::test]
    async fn the_v1_fields_route_answers_like_the_v2_one() {
        let us = MockUs::with_dataset().await;
        let state = us_state(&us).await;
        let (status, v1) = get_json(state.clone(), "/api/data/v1/datasets/dataset-1/fields").await;
        assert_eq!(status, StatusCode::OK);
        let (_, v2) = get_json(state, "/api/data/v2/datasets/dataset-1/fields").await;
        assert_eq!(v1["fields"], v2["fields"]);
        // `getDatasetFields` caches on `revision_id`, so the key is always there.
        assert!(v1.as_object().unwrap().contains_key("revision_id"));
    }

    #[tokio::test]
    async fn a_result_request_compiles_against_the_us_dataset() {
        let us = MockUs::with_dataset().await;
        let state = us_state(&us).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/data/v2/datasets/dataset-1/result")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "fields": [
                                {"ref": {"type": "id", "id": "dim_region"}, "role_spec": {"role": "row"}},
                                {"ref": {"type": "id", "id": "m_sales"}, "role_spec": {"role": "measure"}},
                            ],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let sql = body["blocks"][0]["query"].as_str().unwrap();
        assert!(sql.contains("t0.\"region\""), "{sql}");
        assert!(sql.contains("SUM(t0.\"amount\")"), "{sql}");
        assert!(sql.contains("FROM \"public\".\"orders\" AS t0"), "{sql}");
        assert_eq!(body["fields"][1]["field_type"], "MEASURE");
    }

    #[tokio::test]
    async fn an_unknown_dataset_is_a_404_from_us() {
        let us = MockUs::with_dataset().await;
        let (status, body) =
            get_json(us_state(&us).await, "/api/data/v2/datasets/nope/fields").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "ERR.DS_API.DB.DATASET_NOT_FOUND");
    }

    #[tokio::test]
    async fn the_local_store_still_wins_for_demo_ids() {
        let us = MockUs::with_dataset().await;
        let (status, _) = get_json(us_state(&us).await, "/api/data/v2/datasets/demo/fields").await;
        assert_eq!(status, StatusCode::OK);
        // `demo` never reached `us`.
        assert!(us.requests().is_empty());
    }
}
