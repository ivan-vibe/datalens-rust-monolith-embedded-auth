//! Rust port of the geo (`ymap_wizard_node`) wizard preparers.
//!
//! The JS path is `js/js.ts:buildGraphPrivate` for a `visualization.id` of `geolayer`
//! (a list of layers whose `layerSettings.type` is `geopolygon`, `geopoint`,
//! `geopoint-with-cluster`, `heatmap` or `polyline`). Per layer it runs one of
//! `preparers/{geopolygon.ts,geopoint/index.ts,geopoint-with-cluster.ts,heatmap.ts,polyline.ts}`
//! and returns one `data[]` entry:
//!
//! * geopolygon — `{polygonmap:{polygons:{type:"FeatureCollection",features}},options,bounds}`;
//! * geopoint    — `{collection:{children},options,bounds}`;
//! * cluster     — `{clusterer,options,bounds}`;
//! * heatmap     — `{heatmap,options,bounds}`;
//! * polyline    — `{collection:{children},options,bounds}`.
//!
//! `highchartsConfig` for every geo visualization is the fixed default map state from
//! `highcharts.ts:38-56` plus the union `bounds` (`js.ts:1030`); `config` is the small
//! `buildChartsConfigPrivate` default block (`config/index.ts:97-139`), not the wizard config.
//!
//! The data-api request bodies and the config version migrations are shared with the graph
//! path (`graph_engine::prepare`), so a geo chart asks data-api exactly like the JS engine.
//!
//! Ported scope: coordinates → polygons/points/lines, gradient and palette colours,
//! cluster/size/label/tooltip properties, bounds/state, polyline colours/legend texts and
//! date-formatted sort labels. Not ported (documented in `research/v91/fixes/geo/REPORT.md`):
//! HTML/Markdown tooltips and action params.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use axum::http::{HeaderMap, StatusCode};
use serde_json::{json, Map, Value};

use crate::data_direct;
use crate::engine_main::AppState;
use crate::graph_engine::{self, DatasetInfo, GraphFetched, GraphPrepared};
use crate::us_direct;

const DEFAULT_ICON_COLOR: &str = "rgb(77, 162, 241)";
const DEFAULT_FILL_COLOR: &str = "rgb(77, 162, 241)";
const DEFAULT_POINT_RADIUS: f64 = 2.0;
const DEFAULT_MIN_POINT_RADIUS: f64 = 2.0;
const DEFAULT_MAX_POINT_RADIUS: f64 = 8.0;
const DEFAULT_CHART_LINES_LIMIT: u64 = 100;
/// `GEO_MAP_LAYERS_LEVEL` (`utils/constants.ts`): polygons below polylines below points.
const GEO_Z_INDEX_POLYGON: i64 = 1;
const GEO_Z_INDEX_POLYLINE: i64 = 2;
const GEO_Z_INDEX_GEOPOINT: i64 = 3;
const DEFAULT_MAP_CENTER: [f64; 2] = [55.76, 37.64];
const DEFAULT_MAP_ZOOM: f64 = 8.0;

/// The `/api/run` payload of a geo chart. The caller (`pipeline::run_rust_geo`) wraps it in
/// the same envelope as every other wizard chart.
pub struct GeoAnswer {
    pub data: Value,
    pub config: Value,
    pub library: Value,
    pub sources: Value,
    pub data_export: Value,
    pub dataset_schema: std::sync::Arc<Value>,
    pub forbidden: bool,
    pub latency_ms: u64,
}

/// Build the whole geo answer: migrated config + one data-api request per layer, then the
/// preparer data and the default map library config.
pub async fn run(
    state: &Arc<AppState>,
    shared: &Value,
    params: &Value,
    headers: &HeaderMap,
) -> Result<GeoAnswer, (StatusCode, String)> {
    let dataset_ids = us_direct::dataset_ids(shared);
    let mut datasets = HashMap::new();
    let mut dataset_schema = std::sync::Arc::new(json!([]));
    for dataset in &dataset_ids {
        let (fields, revision) =
            data_direct::fetch_fields(&state.http, &state.data_api, headers, dataset)
                .await
                .map_err(|(status, message)| (status, format!("geo fields: {message}")))?;
        if dataset_schema.as_array().map(Vec::is_empty).unwrap_or(true) {
            dataset_schema = std::sync::Arc::new(fields.clone());
        }
        datasets.insert(
            dataset.clone(),
            DatasetInfo {
                fields: std::sync::Arc::new(fields),
                revision: std::sync::Arc::new(revision),
            },
        );
    }
    let prepared = graph_engine::prepare(shared, params, &datasets)
        .map_err(|message| (StatusCode::NOT_IMPLEMENTED, message))?;
    let fetch_started = Instant::now();
    let mut fetched = Vec::with_capacity(prepared.requests.len());
    let mut data_export = Map::new();
    let mut sources = Map::new();
    let mut forbidden = false;
    for request in &prepared.requests {
        let outcome = data_direct::result(
            &state.http,
            &state.data_api,
            headers,
            &request.dataset,
            &request.body,
        )
        .await
        .map_err(|(status, message)| (status, format!("geo data-api: {message}")))?;
        let status = outcome.status;
        let body = outcome.body;
        if !status.is_success() {
            return Err(data_fetch_error(
                &request.key,
                status.as_u16(),
                &request.body,
                &body,
            ));
        }
        let size = json_len(&body) as u64;
        forbidden |= body.get("data_export_forbidden") == Some(&Value::Bool(true));
        let mut source = Map::new();
        source.insert("sourceId".to_string(), json!(request.key));
        source.insert("sourceType".to_string(), json!("bi_datasets"));
        source.insert("status".to_string(), json!(status.as_u16()));
        source.insert("latency".to_string(), json!(0));
        source.insert("size".to_string(), json!(size));
        source.insert("datasetId".to_string(), json!(request.dataset));
        source.insert("data".to_string(), request.body.clone());
        let query = blocks_query(&body);
        if !query.is_empty() {
            source.insert("info".to_string(), json!({"query": query}));
        }
        sources.insert(request.key.clone(), Value::Object(source));
        if let Some(export) = outcome.data_export {
            data_export.insert(request.key.clone(), export);
        }
        fetched.push(GraphFetched {
            key: request.key.clone(),
            dataset: request.dataset.clone(),
            status: status.as_u16(),
            latency: 0.0,
            size,
            body,
            data_export: None,
        });
    }
    let latency_ms = fetch_started.elapsed().as_millis() as u64;
    let (data, bounds) = build_data(&prepared, &fetched)
        .map_err(|message| (StatusCode::INTERNAL_SERVER_ERROR, message))?;
    let library = geo_library(&prepared.config, bounds);
    let config = geo_config(&prepared.config, &prepared.params);
    Ok(GeoAnswer {
        data,
        config,
        library,
        sources: Value::Object(sources),
        data_export: Value::Object(data_export),
        dataset_schema,
        forbidden,
        latency_ms,
    })
}

// ---------------------------------------------------------------------------------------------
// envelope helpers shared with the graph path (`pipeline.rs`)
// ---------------------------------------------------------------------------------------------

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
}

/// `source.info.query` from a parsed data-api answer (`processor/index.ts:885-910`).
fn blocks_query(body: &Value) -> String {
    body.get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("query").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// The `{error:{...}}` envelope `processor/index.ts:611-644` returns for a failed source.
/// Kept local (the graph path has the same function in `pipeline.rs`) so the geo module does
/// not depend on private pipeline internals.
fn data_fetch_error(
    source_key: &str,
    status: u16,
    request_body: &Value,
    body: &Value,
) -> (StatusCode, String) {
    let code = match status {
        403 => "ENTRY_FORBIDDEN",
        404 => "ENTRY_NOT_FOUND",
        _ => "ERR.CHARTS.DATA_FETCHING_ERROR",
    };
    let mut sources = Map::new();
    sources.insert(
        source_key.to_string(),
        json!({
            "sourceType": "bi_datasets",
            "status": status,
            "data": request_body,
            "body": body,
        }),
    );
    let envelope = json!({
        "error": {
            "code": code,
            "details": {"sources": Value::Object(sources)},
        }
    });
    (
        StatusCode::from_u16(427).unwrap_or(StatusCode::BAD_REQUEST),
        serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".to_string()),
    )
}

// ---------------------------------------------------------------------------------------------
// config / highchartsConfig
// ---------------------------------------------------------------------------------------------

/// `buildChartsConfigPrivate` default block (`config/index.ts:97-139`). Geo visualizations do
/// not add any visualization-specific config, so this is the whole `config` the JS engine sends.
fn geo_config(config: &Value, params: &Value) -> Value {
    let extra = config
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut result = Map::new();
    let title = extra
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty() && extra.get("titleMode") == Some(&json!("show")))
        .map(str::to_string)
        .or_else(|| {
            config
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    if !title.is_empty() {
        result.insert("title".to_string(), json!(title));
    }
    result.insert("hideHolidaysBands".to_string(), json!(true));
    result.insert("linesLimit".to_string(), json!(DEFAULT_CHART_LINES_LIMIT));
    result.insert(
        "tooltip".to_string(),
        json!({"pin": {"altKey": true}, "sort": {"enabled": true}}),
    );
    result.insert("preventDefaultForPointClick".to_string(), json!(false));
    let mut matched_params: Vec<String> = Vec::new();
    if let Some(datasets) = config
        .get("datasetsPartialFields")
        .and_then(Value::as_array)
    {
        for fields in datasets {
            if let Some(fields) = fields.as_array() {
                for field in fields {
                    if let Some(title) = field.get("title").and_then(Value::as_str) {
                        if params.get(title).is_some() && !matched_params.iter().any(|t| t == title)
                        {
                            matched_params.push(title.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut comments = Map::new();
    comments.insert("matchType".to_string(), json!("intersection"));
    comments.insert("matchedParams".to_string(), json!(matched_params));
    if let Some(feed) = extra.get("feed").and_then(Value::as_str) {
        if !feed.is_empty() {
            comments.insert(
                "feeds".to_string(),
                json!([{"feed": feed, "matchedParams": matched_params, "matchType": "intersection"}]),
            );
        }
    }
    result.insert("comments".to_string(), Value::Object(comments));
    if let Some(insights) = extra.get("enableGPTInsights") {
        result.insert("enableGPTInsights".to_string(), insights.clone());
    }
    Value::Object(result)
}

/// Union of every layer's bounds. A missing coordinate stays `null`, exactly like JS
/// `[undefined, undefined]` serializes.
#[derive(Default, Clone, Copy)]
struct Bounds {
    left_lat: Option<f64>,
    left_long: Option<f64>,
    right_lat: Option<f64>,
    right_long: Option<f64>,
}

impl Bounds {
    fn add(&mut self, point: &[f64]) {
        if point.len() < 2 {
            return;
        }
        let (mut lat, mut long) = (point[0], point[1]);
        // `getMapBounds` (`utils/geo-helpers.ts:31-72`): dateline/pole normalisation.
        if lat < -90.0 {
            lat += 180.0;
        }
        if long < -168.0 {
            long += 360.0;
        }
        self.left_lat = Some(self.left_lat.map_or(lat, |value| value.min(lat)));
        self.right_lat = Some(self.right_lat.map_or(lat, |value| value.max(lat)));
        self.left_long = Some(self.left_long.map_or(long, |value| value.min(long)));
        self.right_long = Some(self.right_long.map_or(long, |value| value.max(long)));
        if let Some(left) = self.left_lat.as_mut() {
            if *left < -85.0 {
                *left = -85.0;
            }
        }
    }

    fn merge(&mut self, other: Bounds) {
        if let Some(value) = other.left_lat {
            self.left_lat = Some(self.left_lat.map_or(value, |current| current.min(value)));
        }
        if let Some(value) = other.right_lat {
            self.right_lat = Some(self.right_lat.map_or(value, |current| current.max(value)));
        }
        if let Some(value) = other.left_long {
            self.left_long = Some(self.left_long.map_or(value, |current| current.min(value)));
        }
        if let Some(value) = other.right_long {
            self.right_long = Some(self.right_long.map_or(value, |current| current.max(value)));
        }
    }

    fn is_complete(&self) -> bool {
        self.left_lat.is_some()
            && self.left_long.is_some()
            && self.right_lat.is_some()
            && self.right_long.is_some()
    }

    /// JS `[leftBot, rightTop]` with both `undefined` serializes to a flat `[null, null]`;
    /// a real bound is the two-coordinate pair.
    fn as_value(&self) -> Value {
        if !self.is_complete() {
            return json!([Value::Null, Value::Null]);
        }
        let coordinate = |lat: Option<f64>, long: Option<f64>| {
            json!([
                lat.map_or(Value::Null, number),
                long.map_or(Value::Null, number)
            ])
        };
        json!([
            coordinate(self.left_lat, self.left_long),
            coordinate(self.right_lat, self.right_long)
        ])
    }
}

fn bounds_enabled(config: &Value) -> bool {
    let extra = config.get("extraSettings").unwrap_or(&Value::Null);
    extra.get("zoomMode").and_then(Value::as_str) != Some("manual")
        && extra.get("mapCenterMode").and_then(Value::as_str) != Some("manual")
}

/// `highcharts.ts:38-56` (default geo state) + `getMapState` + the union `bounds`
/// (`js/js.ts:1030-1034`). Bounds, when enabled, are always present — `null` coordinates
/// included — because JS puts `[undefined, undefined]` into the state.
fn geo_library(config: &Value, bounds: Bounds) -> Value {
    let extra = config.get("extraSettings").unwrap_or(&Value::Null);
    let should_set_bounds = bounds_enabled(config);
    let default_center = || json!([DEFAULT_MAP_CENTER[0], DEFAULT_MAP_CENTER[1]]);
    let mut center = default_center();
    if extra.get("mapCenterMode").and_then(Value::as_str) == Some("manual") {
        if let Some(raw) = extra.get("mapCenterValue").and_then(Value::as_str) {
            let parsed: Vec<f64> = raw
                .split(',')
                .filter_map(|part| part.trim().parse::<f64>().ok())
                .collect();
            if parsed.len() == 2 {
                center = json!([number(parsed[0]), number(parsed[1])]);
            }
        }
    } else if !should_set_bounds {
        // Manual zoom but automatic center: midpoint of the data bounds.
        if let (Some(left_lat), Some(left_long), Some(right_lat), Some(right_long)) = (
            bounds.left_lat,
            bounds.left_long,
            bounds.right_lat,
            bounds.right_long,
        ) {
            center = json!([
                number(left_lat / 2.0 + right_lat / 2.0),
                number(left_long / 2.0 + right_long / 2.0)
            ]);
        }
    }
    let zoom = if extra.get("zoomMode").and_then(Value::as_str) == Some("manual") {
        extra
            .get("zoomValue")
            .and_then(Value::as_f64)
            .map(|value| value.max(1.0))
            .unwrap_or(DEFAULT_MAP_ZOOM)
    } else {
        DEFAULT_MAP_ZOOM
    };
    let mut state = Map::new();
    state.insert("center".to_string(), center);
    state.insert("zoom".to_string(), number(zoom));
    state.insert("controls".to_string(), json!(["zoomControl"]));
    state.insert(
        "behaviors".to_string(),
        json!(["drag", "scrollZoom", "multiTouch"]),
    );
    // `js.ts:921-949`: the union is assigned only when at least one layer has real
    // coordinates (`localBoundExists`), so an empty map keeps the default state.
    if should_set_bounds && bounds.is_complete() {
        state.insert("bounds".to_string(), bounds.as_value());
    }
    json!({"state": Value::Object(state), "options": {}})
}

// ---------------------------------------------------------------------------------------------
// data-api response access
// ---------------------------------------------------------------------------------------------

/// One layer's data-api answer, indexed by field guid. The row `legend` defines the order of
/// `row.data` (identical to `graph_engine::response_index_by_guid`).
struct LayerData<'a> {
    rows: &'a [Value],
    indices: HashMap<String, usize>,
    fields: HashMap<String, &'a Value>,
}

impl<'a> LayerData<'a> {
    fn new(body: &'a Value) -> Self {
        let rows = body
            .pointer("/result_data/0/rows")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut fields = HashMap::new();
        if let Some(items) = body.get("fields").and_then(Value::as_array) {
            for field in items {
                if let Some(id) = field.get("id").and_then(Value::as_str) {
                    fields.insert(id.to_string(), field);
                }
            }
        }
        let mut indices = HashMap::new();
        let legend: Vec<i64> = body
            .pointer("/result_data/0/rows/0/legend")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        for (id, field) in &fields {
            let Some(legend_id) = field.get("legend_item_id").and_then(Value::as_i64) else {
                continue;
            };
            if let Some(position) = legend.iter().position(|value| *value == legend_id) {
                indices.insert(id.clone(), position);
            }
        }
        // No rows yet: the field order still defines the future column order.
        if indices.is_empty() {
            let mut ordered: Vec<(&String, &Value)> =
                fields.iter().map(|(id, field)| (id, *field)).collect();
            ordered.sort_by_key(|(_, field)| {
                field
                    .get("legend_item_id")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX)
            });
            for (position, (id, _)) in ordered.into_iter().enumerate() {
                indices.insert(id.clone(), position);
            }
        }
        Self {
            rows,
            indices,
            fields,
        }
    }

    fn field(&self, guid: &str) -> Option<&'a Value> {
        self.fields.get(guid).copied()
    }

    fn data_type(&self, guid: &str, fallback: &str) -> String {
        self.field(guid)
            .and_then(|field| field.get("data_type"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }

    fn title(&self, guid: &str, fallback: &str) -> String {
        self.field(guid)
            .and_then(|field| {
                field
                    .get("title")
                    .or_else(|| field.get("fakeTitle"))
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .unwrap_or_else(|| fallback.to_string())
    }

    fn index(&self, guid: &str) -> Option<usize> {
        self.indices.get(guid).copied()
    }

    fn cell(&self, row: &'a Value, guid: &str) -> &'a Value {
        self.index(guid)
            .and_then(|index| {
                row.get("data")
                    .and_then(Value::as_array)
                    .and_then(|cells| cells.get(index))
            })
            .unwrap_or(&Value::Null)
    }
}

fn is_null_cell(value: &Value) -> bool {
    value.is_null() || value.as_str() == Some("null")
}

/// A cell is a JSON string holding the coordinates/polygon, like the JS preparers
/// (`JSON.parse(columnData)`). Invalid JSON is skipped instead of failing the whole chart
/// (the demo `orders.order_buyer_region_province_polygon` column currently holds names).
fn parse_coordinates(value: &Value) -> Option<Value> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str(text).ok()?,
        Value::Array(_) => value.clone(),
        _ => return None,
    };
    match &parsed {
        Value::Array(_) => Some(parsed),
        _ => None,
    }
}

/// `getFlattenCoordinates` (`acc.concat(val)`): one level down.
fn flatten_coordinates(value: &Value) -> Vec<Value> {
    let mut flattened = Vec::new();
    for item in value.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        match item {
            Value::Array(items) => flattened.extend(items.iter().cloned()),
            other => flattened.push(other.clone()),
        }
    }
    flattened
}

fn coordinate_pair(value: &Value) -> Option<[f64; 2]> {
    let items = value.as_array()?;
    if items.len() < 2 {
        return None;
    }
    Some([items[0].as_f64()?, items[1].as_f64()?])
}

// ---------------------------------------------------------------------------------------------
// layer access
// ---------------------------------------------------------------------------------------------

fn layer_type(layer: &Value) -> String {
    layer
        .pointer("/layerSettings/type")
        .and_then(Value::as_str)
        .or_else(|| layer.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn placeholder<'a>(layer: &'a Value, id: &str) -> Option<&'a Value> {
    layer
        .get("placeholders")
        .and_then(Value::as_array)?
        .iter()
        .find(|placeholder| placeholder.get("id").and_then(Value::as_str) == Some(id))
}

fn placeholder_items<'a>(layer: &'a Value, id: &str) -> &'a [Value] {
    placeholder(layer, id)
        .and_then(|placeholder| placeholder.get("items"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn first_item<'a>(layer: &'a Value, id: &str) -> Option<&'a Value> {
    placeholder_items(layer, id).first()
}

/// The coordinate placeholder of a layer, whatever its type is called in the config.
fn coordinates_item<'a>(layer: &'a Value, layer_id: &str) -> Option<&'a Value> {
    let ids: &[&str] = match layer_id {
        "geopolygon" => &["geopolygon", "geo"],
        "heatmap" => &["heatmap"],
        "polyline" => &["polyline"],
        _ => &["geopoint", "geo"],
    };
    for id in ids {
        if let Some(item) = first_item(layer, id) {
            return Some(item);
        }
    }
    layer
        .get("placeholders")
        .and_then(Value::as_array)
        .and_then(|placeholders| {
            placeholders.iter().find_map(|placeholder| {
                let item = placeholder.get("items")?.as_array()?.first()?;
                let data_type = item.get("data_type").and_then(Value::as_str).unwrap_or("");
                matches!(data_type, "geopoint" | "geopolygon").then_some(item)
            })
        })
}

/// `layer.commonPlaceholders` when the config is layered; the shared config itself otherwise.
fn common<'a>(config: &'a Value, layer: &'a Value) -> &'a Value {
    if let Some(common) = layer.get("commonPlaceholders") {
        return common;
    }
    if layer.get("layerSettings").is_some() || layer.get("commonPlaceholders").is_some() {
        return layer;
    }
    config
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn first_field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    array(value, key).first()
}

fn field_guid(item: &Value) -> &str {
    item.get("guid").and_then(Value::as_str).unwrap_or("")
}

fn field_title(item: &Value) -> String {
    item.get("fakeTitle")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .or_else(|| item.get("title").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

/// `JSON.stringify` for a JS `Number`: integral doubles stay integers.
fn number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn layer_alpha(layer: &Value) -> f64 {
    let alpha = layer
        .pointer("/layerSettings/alpha")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if alpha == 0.0 {
        0.8
    } else {
        alpha * 1e-2
    }
}

// ---------------------------------------------------------------------------------------------
// colours (`utils/color-helpers.ts`, `utils/geo-helpers.ts`)
// ---------------------------------------------------------------------------------------------

fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

/// `selectAvailableGradientsColors`: system gradient palettes used by the demo corpus plus the
/// per-mode defaults. A palette outside this table falls back to the mode default, like JS.
fn gradient_colors(mode: &str, palette: &str) -> Vec<(u8, u8, u8)> {
    let three_point = mode == "3-point";
    let hexes: &[&str] = match palette {
        "orange-gray-blue" => &["#FF9580", "#D8D8D8", "#8CCBFF"],
        "red-orange-green" => &["#FF3D64", "#FFC636", "#54A520"],
        "blue" => &["#0044A3", "#8CCBFF"],
        _ if three_point => &["#FF3D64", "#FFC636", "#54A520"],
        _ => &["#0044A3", "#8CCBFF"],
    };
    hexes.iter().filter_map(|hex| hex_to_rgb(hex)).collect()
}

fn gradient_palette_id(colors_config: &Value) -> String {
    let mode = colors_config
        .get("gradientMode")
        .and_then(Value::as_str)
        .unwrap_or("2-point");
    colors_config
        .get("gradientPalette")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if mode == "3-point" {
                "red-orange-green".to_string()
            } else {
                "blue".to_string()
            }
        })
}

/// `getThresholdValues` (`color-helpers.ts:435-464`).
struct Thresholds {
    min: f64,
    range: f64,
    range_middle: f64,
    max: f64,
}

fn thresholds(colors_config: &Value, values: &[f64]) -> Option<Thresholds> {
    if values.is_empty() {
        return None;
    }
    let manual = colors_config.get("thresholdsMode").and_then(Value::as_str) == Some("manual");
    let max = if manual {
        colors_config.get("rightThreshold").and_then(Value::as_f64)
    } else {
        None
    }
    .unwrap_or_else(|| values.iter().copied().fold(f64::NEG_INFINITY, f64::max));
    let min = if manual {
        colors_config.get("leftThreshold").and_then(Value::as_f64)
    } else {
        None
    }
    .unwrap_or_else(|| values.iter().copied().fold(f64::INFINITY, f64::min));
    let range = max - min;
    let three_point = colors_config.get("gradientMode").and_then(Value::as_str) == Some("3-point");
    let mid = if manual && three_point {
        colors_config.get("middleThreshold").and_then(Value::as_f64)
    } else {
        None
    }
    .unwrap_or(max - range / 2.0);
    let range_middle = if mid == range / 2.0 { mid } else { mid - min };
    Some(Thresholds {
        min,
        range,
        range_middle,
        max,
    })
}

/// `getRgbColorValue` (`shared/modules/wizard-helpers.ts:156-184`).
fn rgb_color_value(
    delta: f64,
    mode: &str,
    range_middle_ratio: f64,
    colors: &[(u8, u8, u8)],
) -> String {
    let (mut result_delta, shade_a, shade_b) = if mode == "3-point" && colors.len() >= 3 {
        if delta >= range_middle_ratio {
            (
                if delta == 1.0 {
                    delta
                } else {
                    delta - range_middle_ratio
                },
                colors[1],
                colors[2],
            )
        } else {
            (delta, colors[0], colors[1])
        }
    } else if colors.len() >= 2 {
        (delta, colors[0], colors[1])
    } else {
        (delta, (0, 0, 0), (0, 0, 0))
    };
    if !result_delta.is_finite() {
        result_delta = 0.0;
    }
    let channel =
        |a: u8, b: u8| -> i64 { ((b as f64 - a as f64) * result_delta + a as f64).floor() as i64 };
    format!(
        "rgb({}, {}, {})",
        channel(shade_a.0, shade_b.0),
        channel(shade_a.1, shade_b.1),
        channel(shade_a.2, shade_b.2)
    )
}

fn system_palette_colors(palette: &str, default_palette: &str) -> Vec<String> {
    let palettes = graph_engine::system_palettes();
    let chosen = if palette.is_empty() {
        default_palette
    } else {
        palette
    };
    palettes
        .get(chosen)
        .and_then(|palette| palette.get("scheme"))
        .and_then(Value::as_array)
        .map(|scheme| {
            scheme
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|colors: &Vec<String>| !colors.is_empty())
        .unwrap_or_else(|| {
            palettes
                .get(default_palette)
                .and_then(|palette| palette.get("scheme"))
                .and_then(Value::as_array)
                .map(|scheme| {
                    scheme
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        })
}

fn mount_color(mounted: &Map<String, Value>, colors: &[String], value: &str) -> Option<String> {
    let mounted = mounted.get(value)?;
    let Some(index) = mounted.as_f64() else {
        return mounted.as_str().map(str::to_string);
    };
    if colors.is_empty() {
        return None;
    }
    Some(colors[index as usize % colors.len()].clone())
}

/// Colours resolved for one layer's colour field. `by_value` maps the raw cell string to
/// `(css colour, colorIndex-or-measure-value)`; `dictionary` is the dimension legend.
struct ColorScale {
    by_value: HashMap<String, (String, f64)>,
    dictionary: BTreeMap<String, String>,
    gradient: bool,
    mode: String,
    palette_id: String,
    min: Option<f64>,
    mid: Option<f64>,
    max: Option<f64>,
}

fn build_color_scale(
    data: &LayerData,
    color_item: &Value,
    colors_config: &Value,
    default_palette: &str,
) -> ColorScale {
    let mut scale = ColorScale {
        by_value: HashMap::new(),
        dictionary: BTreeMap::new(),
        gradient: false,
        mode: colors_config
            .get("gradientMode")
            .and_then(Value::as_str)
            .unwrap_or("2-point")
            .to_string(),
        palette_id: gradient_palette_id(colors_config),
        min: None,
        mid: None,
        max: None,
    };
    let guid = field_guid(color_item);
    let data_type = data.data_type(guid, "");
    let numerical = matches!(data_type.as_str(), "integer" | "uinteger" | "float");
    scale.gradient = color_item.get("type").and_then(Value::as_str) == Some("MEASURE")
        || numerical && colors_config.get("colorMode").and_then(Value::as_str) == Some("gradient");

    let mut raw_values: Vec<(String, Option<f64>)> = Vec::new();
    for row in data.rows {
        let cell = data.cell(row, guid);
        if is_null_cell(cell) {
            continue;
        }
        let text = value_text(cell);
        if scale.gradient {
            if let Ok(number) = text.parse::<f64>() {
                raw_values.push((text, Some(number)));
            }
        } else if !raw_values.iter().any(|(known, _)| known == &text) {
            raw_values.push((text, None));
        }
    }
    if scale.gradient {
        let numbers: Vec<f64> = raw_values
            .iter()
            .filter_map(|(_, number)| *number)
            .collect();
        let Some(thresholds) = thresholds(colors_config, &numbers) else {
            return scale;
        };
        let reversed = colors_config
            .get("reversed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut colors = gradient_colors(&scale.mode, &scale.palette_id);
        if reversed {
            colors.reverse();
        }
        let ratio = if thresholds.range == 0.0 {
            0.0
        } else {
            thresholds.range_middle / thresholds.range
        };
        for (text, number) in &raw_values {
            let Some(number) = number else { continue };
            let delta = if thresholds.range == 0.0 {
                0.5
            } else {
                ((number - thresholds.min) / thresholds.range).clamp(0.0, 1.0)
            };
            scale.by_value.insert(
                text.clone(),
                (rgb_color_value(delta, &scale.mode, ratio, &colors), *number),
            );
        }
        scale.min = Some(thresholds.min);
        scale.max = Some(thresholds.max);
        scale.mid = Some(thresholds.min + thresholds.range_middle);
        return scale;
    }
    let colors_config_mounted = colors_config
        .get("mountedColors")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mounted_matches = !colors_config_mounted.is_empty()
        && (colors_config.get("fieldGuid").and_then(Value::as_str) == Some(guid)
            || colors_config
                .get("coloredByMeasure")
                .and_then(Value::as_bool)
                == Some(true));
    let palette_id = colors_config
        .get("palette")
        .and_then(Value::as_str)
        .unwrap_or(default_palette);
    let palette = system_palette_colors(palette_id, default_palette);
    for (index, (text, _)) in raw_values.iter().enumerate() {
        let color = if mounted_matches {
            mount_color(&colors_config_mounted, &palette, text)
        } else {
            None
        }
        .unwrap_or_else(|| {
            if palette.is_empty() {
                DEFAULT_ICON_COLOR.to_string()
            } else {
                palette[index % palette.len()].clone()
            }
        });
        scale
            .by_value
            .insert(text.clone(), (color.clone(), index as f64));
        scale.dictionary.insert(text.clone(), color);
    }
    scale
}

fn gradient_data(scale: &ColorScale) -> Value {
    let colors: Vec<Value> = gradient_colors(&scale.mode, &scale.palette_id)
        .into_iter()
        .map(|(red, green, blue)| json!({"red": red, "green": green, "blue": blue}))
        .collect();
    json!({"id": scale.palette_id, "colors": colors})
}

fn gradient_options(scale: &ColorScale, color_title: &str) -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("mode".to_string(), json!("gradient"));
    options.insert("colorTitle".to_string(), json!(color_title));
    options.insert("gradientData".to_string(), gradient_data(scale));
    options.insert(
        "colorMinValue".to_string(),
        scale.min.map_or(Value::Null, number),
    );
    options.insert(
        "colorMaxValue".to_string(),
        scale.max.map_or(Value::Null, number),
    );
    if scale.mode == "3-point" {
        options.insert(
            "colorMidValue".to_string(),
            scale.mid.map_or(Value::Null, number),
        );
    }
    options
}

// ---------------------------------------------------------------------------------------------
// tooltips
// ---------------------------------------------------------------------------------------------

struct TooltipContext<'a> {
    fields: &'a [Value],
    data: &'a LayerData<'a>,
    title_mode: bool,
}

impl<'a> TooltipContext<'a> {
    fn new(common: &'a Value, data: &'a LayerData<'a>) -> Self {
        let tooltip_config = common.get("tooltipConfig").unwrap_or(&Value::Null);
        Self {
            fields: array(common, "tooltips"),
            data,
            title_mode: tooltip_config.get("fieldTitle").and_then(Value::as_str) != Some("off"),
        }
    }

    /// `properties.data[index]` of a geo feature: one entry per tooltip field.
    fn entries(&self, row: &Value) -> Vec<Option<Value>> {
        self.fields
            .iter()
            .map(|field| {
                let guid = field_guid(field);
                let cell = self.data.cell(row, guid);
                if is_null_cell(cell) {
                    return None;
                }
                let text = value_text(cell);
                let title = if self.title_mode {
                    field_title(field)
                } else {
                    String::new()
                };
                let text = if title.is_empty() {
                    text
                } else {
                    format!("{title}: {text}")
                };
                Some(json!({"text": text}))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// data building
// ---------------------------------------------------------------------------------------------

struct LayerOutput {
    value: Value,
    bounds: Bounds,
}

/// Build every layer's `data` entry and the union bounds.
fn build_data(
    prepared: &GraphPrepared,
    fetched: &[GraphFetched],
) -> Result<(Value, Bounds), String> {
    let config = &prepared.config;
    let visualization = config
        .get("visualization")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let layers = array(&visualization, "layers").to_vec();
    let layered = !layers.is_empty();
    let layer_values: Vec<Value> = if layered {
        layers
    } else {
        vec![visualization.clone()]
    };
    let default_palette = "default20";
    let mut outputs = Vec::with_capacity(layer_values.len());
    let mut bounds = Bounds::default();
    for (index, layer) in layer_values.iter().enumerate() {
        let fetched_layer = fetched
            .get(index)
            .ok_or_else(|| "geo: data-api answer is missing for a layer".to_string())?;
        let data = LayerData::new(&fetched_layer.body);
        let layer_id = if layered {
            layer_type(layer)
        } else {
            geo_visualization_id(&visualization)
        };
        let output = build_layer(config, layer, &data, &layer_id, layered, default_palette)?;
        bounds.merge(output.bounds);
        outputs.push(output.value);
    }
    Ok((Value::Array(outputs), bounds))
}

fn geo_visualization_id(visualization: &Value) -> String {
    let id = visualization
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("geopoint");
    if id != "geolayer" {
        return id.to_string();
    }
    let placeholders = visualization
        .get("placeholders")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for placeholder in placeholders {
        match placeholder.get("id").and_then(Value::as_str) {
            Some("geopolygon") => return "geopolygon".to_string(),
            Some("heatmap") => return "heatmap".to_string(),
            Some("polyline") => return "polyline".to_string(),
            Some("geopoint") => return "geopoint".to_string(),
            _ => {}
        }
    }
    "geopoint".to_string()
}

#[allow(clippy::too_many_arguments)]
fn build_layer(
    config: &Value,
    layer: &Value,
    data: &LayerData,
    layer_id: &str,
    _layered: bool,
    default_palette: &str,
) -> Result<LayerOutput, String> {
    let shared_common = common(config, layer);
    let colors = array(shared_common, "colors");
    let color_item = colors.first();
    let colors_config = shared_common.get("colorsConfig").unwrap_or(&Value::Null);
    let scale =
        color_item.map(|item| build_color_scale(data, item, colors_config, default_palette));
    let coordinates = coordinates_item(layer, layer_id)
        .ok_or_else(|| format!("geo: layer {layer_id} has no coordinate placeholder"))?;
    let coordinate_guid = field_guid(coordinates);
    let legend_hidden = config
        .pointer("/extraSettings/legendMode")
        .and_then(Value::as_str)
        == Some("hide");
    let tooltips = TooltipContext::new(shared_common, data);
    let alpha = layer_alpha(layer);
    let output = match layer_id {
        "geopolygon" => build_geopolygon(
            layer,
            shared_common,
            data,
            coordinates,
            coordinate_guid,
            color_item,
            scale.as_ref(),
            &tooltips,
            alpha,
            legend_hidden,
        ),
        "geopoint" | "geopoint-with-cluster" => build_geopoint(
            layer,
            shared_common,
            data,
            coordinates,
            coordinate_guid,
            color_item,
            scale.as_ref(),
            &tooltips,
            alpha,
            layer_id == "geopoint-with-cluster",
            legend_hidden,
        ),
        "heatmap" => build_heatmap(
            layer,
            data,
            coordinates,
            coordinate_guid,
            color_item,
            scale.as_ref(),
            alpha,
            legend_hidden,
        ),
        "polyline" => {
            build_polyline(config, layer, shared_common, data, coordinate_guid, alpha, default_palette)
        }
        other => return Err(format!("geo: layer type {other} is not ported")),
    };
    Ok(output)
}

/// Options shared by every geo layer (`geopoint`/`cluster`/`polyline` base).
fn base_options(layer: &Value, alpha: f64, legend_hidden: bool) -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("opacity".to_string(), number(alpha));
    options.insert("showCustomLegend".to_string(), json!(!legend_hidden));
    if let Some(id) = layer
        .pointer("/layerSettings/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        options.insert("geoObjectId".to_string(), json!(id));
    }
    let name = layer
        .pointer("/layerSettings/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Layer 1");
    options.insert("layerTitle".to_string(), json!(name));
    options
}

/// `geopolygon` map options (`geopolygon.ts:396-455`).
fn polygon_options(layer: &Value, alpha: f64, legend_hidden: bool) -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("fillOpacity".to_string(), number(alpha));
    options.insert(
        "fillOpacityHover".to_string(),
        number((alpha + 0.1).min(1.0)),
    );
    options.insert(
        "fillColorEmptyPolygon".to_string(),
        json!(DEFAULT_FILL_COLOR),
    );
    options.insert("strokeColorHover".to_string(), json!("#FFF"));
    options.insert("strokeWidthHover".to_string(), json!(2));
    options.insert("showCustomLegend".to_string(), json!(!legend_hidden));
    if legend_hidden {
        options.insert("showLegend".to_string(), json!(false));
    }
    if let Some(id) = layer
        .pointer("/layerSettings/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        options.insert("geoObjectId".to_string(), json!(id));
    }
    let name = layer
        .pointer("/layerSettings/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Layer 1");
    options.insert("layerTitle".to_string(), json!(name));
    options
}

fn color_title(color_item: &Value, data: &LayerData) -> String {
    let guid = field_guid(color_item);
    let fallback = field_title(color_item);
    data.title(guid, &fallback)
}

fn feature_data(tooltips: &TooltipContext, row: &Value, color: Option<&str>) -> Vec<Value> {
    let entries = tooltips.entries(row);
    let mut data: Vec<Value> = entries
        .into_iter()
        .map(|entry| entry.unwrap_or(Value::Null))
        .collect();
    if let Some(color) = color {
        if let Some(first) = data.first_mut().and_then(Value::as_object_mut) {
            first.insert("color".to_string(), json!(color));
        }
    }
    while data.last().is_some_and(Value::is_null) {
        data.pop();
    }
    data
}

// --- geopolygon ------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_geopolygon(
    layer: &Value,
    common: &Value,
    data: &LayerData,
    coordinates: &Value,
    coordinate_guid: &str,
    color_item: Option<&Value>,
    scale: Option<&ColorScale>,
    tooltips: &TooltipContext,
    alpha: f64,
    legend_hidden: bool,
) -> LayerOutput {
    let column_index = data.index(coordinate_guid).unwrap_or(0);
    let colors_config = common.get("colorsConfig").unwrap_or(&Value::Null);
    let borders_hidden =
        colors_config.get("polygonBorders").and_then(Value::as_str) == Some("hide");
    let mut bounds = Bounds::default();
    let mut features = Vec::new();
    for row in data.rows {
        let raw = data.cell(row, coordinate_guid);
        if is_null_cell(raw) {
            continue;
        }
        let Some(polygon) = parse_coordinates(raw) else {
            continue;
        };
        for point in flatten_coordinates(&polygon) {
            if let Some(pair) = coordinate_pair(&point) {
                bounds.add(&pair);
            }
        }
        let color = color_item.and_then(|item| {
            let cell = data.cell(row, field_guid(item));
            if is_null_cell(cell) {
                return None;
            }
            let key = value_text(cell);
            scale.and_then(|scale| {
                scale
                    .by_value
                    .get(&key)
                    .map(|(color, index)| (color.clone(), *index))
            })
        });
        let mut options = Map::new();
        options.insert("strokeColor".to_string(), json!("#FFF"));
        options.insert("zIndex".to_string(), json!(GEO_Z_INDEX_POLYGON));
        let mut properties = Map::new();
        if let Some((color, color_index)) = &color {
            options.insert("fillColor".to_string(), json!(color));
            options.insert("fillColorDefault".to_string(), json!(color));
            options.insert("iconColor".to_string(), json!(color));
            properties.insert("colorIndex".to_string(), number(*color_index));
            if borders_hidden {
                options.insert("strokeWidth".to_string(), json!(0));
            }
        }
        let data_entries = feature_data(
            tooltips,
            row,
            color.as_ref().map(|(color, _)| color.as_str()),
        );
        if !data_entries.is_empty() {
            properties.insert("data".to_string(), Value::Array(data_entries));
        }
        let _ = coordinates;
        features.push(json!({
            "geometry": {"type": "Polygon", "coordinates": polygon},
            "properties": Value::Object(properties),
            "options": Value::Object(options),
            "columnIndex": column_index,
            "type": "Feature",
        }));
    }
    let mut options = polygon_options(layer, alpha, legend_hidden);
    if let (Some(color_item), Some(scale)) = (color_item, scale) {
        let title = color_title(color_item, data);
        if scale.gradient {
            options.extend(gradient_options(scale, &title));
        } else {
            options.insert("mode".to_string(), json!("dictionary"));
            options.insert("colorTitle".to_string(), json!(title));
            options.insert(
                "colorDictionary".to_string(),
                Value::Object(
                    scale
                        .dictionary
                        .clone()
                        .into_iter()
                        .map(|(key, value)| (key, json!(value)))
                        .collect(),
                ),
            );
        }
    }
    LayerOutput {
        value: json!({
            "polygonmap": {"polygons": {"type": "FeatureCollection", "features": features}},
            "options": Value::Object(options),
            "bounds": bounds.as_value(),
        }),
        bounds,
    }
}

// --- geopoint / cluster ----------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_geopoint(
    layer: &Value,
    common: &Value,
    data: &LayerData,
    coordinates: &Value,
    coordinate_guid: &str,
    color_item: Option<&Value>,
    scale: Option<&ColorScale>,
    tooltips: &TooltipContext,
    alpha: f64,
    clustered: bool,
    legend_hidden: bool,
) -> LayerOutput {
    let size_item = first_item(layer, "size");
    let size_guid = size_item.map(field_guid).unwrap_or("");
    let mut size_min: Option<f64> = None;
    let mut size_max: Option<f64> = None;
    if size_item.is_some() {
        for row in data.rows {
            let cell = data.cell(row, size_guid);
            if is_null_cell(cell) {
                continue;
            }
            let Ok(value) = value_text(cell).parse::<f64>() else {
                continue;
            };
            extreme(&mut size_min, &mut size_max, value);
        }
    }
    let label_item = first_field(common, "labels");
    let geopoints_config = common
        .get("geopointsConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let column_index = data.index(coordinate_guid).unwrap_or(0);
    let mut bounds = Bounds::default();
    let mut children = Vec::new();
    for row in data.rows {
        let raw = data.cell(row, coordinate_guid);
        if is_null_cell(raw) {
            continue;
        }
        let Some(point) = parse_coordinates(raw) else {
            continue;
        };
        let Some(pair) = coordinate_pair(&point) else {
            continue;
        };
        bounds.add(&pair);
        let mut radius = DEFAULT_POINT_RADIUS;
        if size_item.is_some() {
            if let (Some(min), Some(max)) = (size_min, size_max) {
                if let Ok(value) = value_text(data.cell(row, size_guid)).parse::<f64>() {
                    radius = point_radius(value, min, max, &geopoints_config);
                }
            }
        }
        let mut properties = Map::new();
        properties.insert("radius".to_string(), number(radius));
        properties.insert("rawText".to_string(), json!(true));
        if let Some(label) = label_item {
            let cell = data.cell(row, field_guid(label));
            if !is_null_cell(cell) {
                properties.insert("label".to_string(), json!(value_text(cell)));
            }
        }
        let color = color_item.and_then(|item| {
            let cell = data.cell(row, field_guid(item));
            if is_null_cell(cell) {
                return None;
            }
            let key = value_text(cell);
            scale.and_then(|scale| scale.by_value.get(&key).map(|(color, _)| color.clone()))
        });
        let data_entries = feature_data(tooltips, row, color.as_deref());
        if !data_entries.is_empty() {
            properties.insert("data".to_string(), Value::Array(data_entries));
        }
        children.push(json!({
            "feature": {
                "geometry": {"type": "Point", "coordinates": point},
                "properties": Value::Object(properties),
            },
            "options": {
                "iconColor": color.unwrap_or_else(|| DEFAULT_ICON_COLOR.to_string()),
                "preset": "chartkit#chips",
                "zIndex": GEO_Z_INDEX_GEOPOINT,
            },
            "columnIndex": column_index,
        }));
    }
    let mut options = base_options(layer, alpha, legend_hidden);
    if let (Some(size_item), Some(min), Some(max)) = (size_item, size_min, size_max) {
        options.insert("sizeMinValue".to_string(), number(min));
        options.insert("sizeMaxValue".to_string(), number(max));
        options.insert(
            "sizeTitle".to_string(),
            json!(data.title(field_guid(size_item), &field_title(size_item))),
        );
    }
    if let (Some(color_item), Some(scale)) = (color_item, scale) {
        let title = color_title(color_item, data);
        if scale.gradient {
            options.extend(gradient_options(scale, &title));
        } else {
            options.insert("mode".to_string(), json!("dictionary"));
            options.insert("colorTitle".to_string(), json!(title));
            options.insert(
                "colorDictionary".to_string(),
                Value::Object(
                    scale
                        .dictionary
                        .clone()
                        .into_iter()
                        .map(|(key, value)| (key, json!(value)))
                        .collect(),
                ),
            );
        }
    }
    let _ = coordinates;
    if clustered {
        options.insert("clusterIconLayout".to_string(), json!("default#pieChart"));
        options.insert("iconPieChartCoreRadius".to_string(), json!(15));
        options.insert("iconPieChartRadius".to_string(), json!(20));
        options.insert("iconPieChartStrokeWidth".to_string(), json!(1));
        options.insert("hasBalloon".to_string(), json!(false));
        options.insert("margin".to_string(), json!(20));
        LayerOutput {
            value: json!({
                "clusterer": children,
                "options": Value::Object(options),
                "bounds": bounds.as_value(),
            }),
            bounds,
        }
    } else {
        LayerOutput {
            value: json!({
                "options": Value::Object(options),
                "bounds": bounds.as_value(),
                "collection": {"children": children},
            }),
            bounds,
        }
    }
}

fn extreme(min: &mut Option<f64>, max: &mut Option<f64>, value: f64) {
    let mut current_min = match *min {
        Some(existing) if existing != 0.0 => existing,
        _ => value,
    };
    let mut current_max = match *max {
        Some(existing) if existing != 0.0 => existing,
        _ => value,
    };
    if value < current_min {
        current_min = value;
    }
    if value > current_max {
        current_max = value;
    }
    *min = Some(current_min);
    *max = Some(current_max);
}

/// `getPointRadius` (`utils/misc-helpers.ts:260-275`).
fn point_radius(current: f64, min: f64, max: f64, geopoints_config: &Value) -> f64 {
    let min_radius = geopoints_config
        .get("minRadius")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or(DEFAULT_MIN_POINT_RADIUS);
    let max_radius = geopoints_config
        .get("maxRadius")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or(DEFAULT_MAX_POINT_RADIUS);
    let mut denominator = max - min;
    if denominator == 0.0 {
        denominator = 1.0;
    }
    let coeff = (max_radius - min_radius) / denominator;
    min_radius + (current - min) * if coeff.is_nan() { 0.0 } else { coeff }
}

// --- heatmap ---------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_heatmap(
    layer: &Value,
    data: &LayerData,
    coordinates: &Value,
    coordinate_guid: &str,
    color_item: Option<&Value>,
    scale: Option<&ColorScale>,
    alpha: f64,
    legend_hidden: bool,
) -> LayerOutput {
    let color_guid = color_item.map(field_guid).unwrap_or("");
    let mut min: Option<f64> = None;
    let mut max: Option<f64> = None;
    if color_item.is_some() {
        for row in data.rows {
            let cell = data.cell(row, color_guid);
            if is_null_cell(cell) {
                continue;
            }
            if let Ok(value) = value_text(cell).parse::<f64>() {
                extreme(&mut min, &mut max, value);
            }
        }
    }
    let step = match (min, max) {
        (Some(min), Some(max)) => Some((max - min) / 10.0),
        _ => None,
    };
    let mut bounds = Bounds::default();
    let mut points: Vec<Value> = Vec::new();
    for row in data.rows {
        let raw = data.cell(row, coordinate_guid);
        if is_null_cell(raw) {
            continue;
        }
        let Some(point) = parse_coordinates(raw) else {
            continue;
        };
        let Some(pair) = coordinate_pair(&point) else {
            continue;
        };
        bounds.add(&pair);
        let mut weight = 5.0;
        if let (Some(color_item), Some(step), Some(min)) = (color_item, step, min) {
            let cell = data.cell(row, field_guid(color_item));
            if !is_null_cell(cell) {
                if let Ok(value) = value_text(cell).parse::<f64>() {
                    if step != 0.0 {
                        let calculated = ((value - min) / step).ceil();
                        weight = if calculated != 0.0 { calculated } else { 1.0 };
                    }
                }
            }
        }
        points.push(json!({
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": point},
            "properties": {"weight": number(weight)},
        }));
    }
    let mut options = Map::new();
    options.insert("radius".to_string(), json!(15));
    options.insert("dissipating".to_string(), json!(false));
    options.insert("opacity".to_string(), number(alpha));
    let custom_palette = color_item.is_some();
    let (gradient, intensity) = heatmap_gradient(color_item, scale, custom_palette);
    options.insert("intensityOfMidpoint".to_string(), number(intensity));
    options.insert("gradient".to_string(), gradient);
    options.insert("isCustomPalette".to_string(), json!(custom_palette));
    options.insert("showCustomLegend".to_string(), json!(true));
    let title = data.title(coordinate_guid, &field_title(coordinates));
    options.insert("colorTitle".to_string(), json!(title));
    if let Some(id) = layer
        .pointer("/layerSettings/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        options.insert("geoObjectId".to_string(), json!(id));
    }
    let name = layer
        .pointer("/layerSettings/name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Layer 1");
    options.insert("layerTitle".to_string(), json!(name));
    if legend_hidden {
        options.insert("showCustomLegend".to_string(), json!(false));
    }
    LayerOutput {
        value: json!({
            "heatmap": points,
            "options": Value::Object(options),
            "bounds": bounds.as_value(),
        }),
        bounds,
    }
}

fn heatmap_gradient(
    color_item: Option<&Value>,
    scale: Option<&ColorScale>,
    custom_palette: bool,
) -> (Value, f64) {
    let default_gradient = json!({
        "0.1": "rgba(128, 255, 0, 0.7)",
        "0.2": "rgba(255, 255, 0, 0.8)",
        "0.7": "rgba(234, 72, 58, 0.9)",
        "1.0": "rgba(162, 36, 25, 1)",
    });
    let Some(scale) = scale else {
        return (default_gradient, 0.2);
    };
    if !custom_palette || color_item.is_none() {
        return (default_gradient, 0.2);
    }
    let colors = gradient_colors(&scale.mode, &scale.palette_id);
    let mut middle = 0.2;
    if colors.len() >= 3 {
        let manual = scale.mode == "3-point";
        if manual {
            if let (Some(min), Some(max)) = (scale.min, scale.max) {
                let range = max - min;
                let mid = scale.mid.unwrap_or(max - range / 2.0);
                let range_middle = if mid == range / 2.0 { mid } else { mid - min };
                if range != 0.0 {
                    middle = range_middle / range;
                }
            }
        }
        let gradient = json!({
            "0": format!("rgba({}, {}, {}, 0.7)", colors[0].0, colors[0].1, colors[0].2),
            "0.5": format!("rgba({}, {}, {}, 0.9)", colors[1].0, colors[1].1, colors[1].2),
            "1": format!("rgba({}, {}, {}, 1)", colors[2].0, colors[2].1, colors[2].2),
        });
        (gradient, middle)
    } else {
        let gradient = json!({
            "0": format!("rgba({}, {}, {}, 0.7)", colors[0].0, colors[0].1, colors[0].2),
            "1": format!("rgba({}, {}, {}, 1)", colors[1].0, colors[1].1, colors[1].2),
        });
        (gradient, middle)
    }
}

// --- polyline --------------------------------------------------------------------------------

fn is_date_field(field: &Value) -> bool {
    matches!(
        field.get("data_type").and_then(Value::as_str).unwrap_or(""),
        "date" | "datetime" | "genericdatetime" | "datetimetz"
    )
}

fn polyline_field_title(field: &Value, data: &LayerData) -> String {
    let title = field.get("title").and_then(Value::as_str).unwrap_or("");
    if title.is_empty() {
        data.title(field_guid(field), "")
    } else {
        title.to_string()
    }
}

/// JS array interpolation (`${[lat, long]}`) is `Array#toString`: comma-joined numbers.
fn coordinate_text(pair: &[f64; 2]) -> String {
    format!("{},{}", js_number_text(pair[0]), js_number_text(pair[1]))
}

fn js_number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        (value as i64).to_string()
    } else {
        format!("{value}")
    }
}

fn build_polyline(
    config: &Value,
    layer: &Value,
    common: &Value,
    data: &LayerData,
    coordinate_guid: &str,
    alpha: f64,
    default_palette: &str,
) -> LayerOutput {
    let grouping: Vec<Value> = placeholder_items(layer, "grouping").to_vec();
    let measures: Vec<Value> = placeholder_items(layer, "measures").to_vec();
    let show_points = placeholder(layer, "polyline")
        .and_then(|placeholder| placeholder.get("settings"))
        .and_then(|settings| settings.get("polylinePoints"))
        .and_then(Value::as_str)
        == Some("on");
    let color_field = array(common, "colors").first();
    let color_scale = color_field.map(|field| {
        build_color_scale(
            data,
            field,
            common.get("colorsConfig").unwrap_or(&Value::Null),
            default_palette,
        )
    });
    let sort_field = array(common, "sort").first();
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    let mut bounds = Bounds::default();
    for row in data.rows {
        let raw = data.cell(row, coordinate_guid);
        if is_null_cell(raw) {
            continue;
        }
        let Some(point) = parse_coordinates(raw) else {
            continue;
        };
        let Some(pair) = coordinate_pair(&point) else {
            continue;
        };
        bounds.add(&pair);
        let group = grouping
            .iter()
            .map(|field| {
                format!(
                    "{}: {}",
                    polyline_field_title(field, data),
                    value_text(data.cell(row, field_guid(field)))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let group = if group.is_empty() {
            "DEFAULT_GROUP".to_string()
        } else {
            group
        };
        let position = *group_index.entry(group.clone()).or_insert_with(|| {
            groups.push((group.clone(), Vec::new()));
            groups.len() - 1
        });
        groups[position].1.push(row.clone());
    }

    let color_of = |row: &Value| -> Option<String> {
        let field = color_field?;
        let scale = color_scale.as_ref()?;
        let value = value_text(data.cell(row, field_guid(field)));
        scale.by_value.get(&value).map(|(color, _)| color.clone())
    };
    let color_text = |row: &Value| -> Option<String> {
        let field = color_field?;
        Some(format!(
            "{}: {}",
            polyline_field_title(field, data),
            value_text(data.cell(row, field_guid(field)))
        ))
    };
    let sort_text = |row: &Value| -> Option<String> {
        let field = sort_field?;
        let value = data.cell(row, field_guid(field));
        Some(if is_date_field(field) {
            crate::gravity_donut::format_date(
                value,
                field.get("data_type").and_then(Value::as_str).unwrap_or(""),
                field.get("format").and_then(Value::as_str).unwrap_or(""),
            )
        } else {
            value_text(value)
        })
    };
    let measure_values = |row: &Value| -> Vec<String> {
        measures
            .iter()
            .map(|measure| value_text(data.cell(row, field_guid(measure))))
            .collect()
    };
    let measure_text = |row: &Value| -> Vec<String> {
        measures
            .iter()
            .map(|measure| {
                format!(
                    "{}: {}",
                    polyline_field_title(measure, data),
                    value_text(data.cell(row, field_guid(measure)))
                )
            })
            .collect()
    };

    let mut children = Vec::new();
    for (group, rows) in &groups {
        let mut points: Vec<([f64; 2], &Value)> = Vec::new();
        for row in rows {
            let raw = data.cell(row, coordinate_guid);
            let Some(point) = parse_coordinates(raw) else {
                continue;
            };
            let Some(pair) = coordinate_pair(&point) else {
                continue;
            };
            points.push((pair, row));
        }
        for window in points.windows(2) {
            let (previous, previous_row) = window[0];
            let (current, _) = window[1];
            let color = color_of(previous_row);
            let mut tooltip = Vec::new();
            if group != "DEFAULT_GROUP" {
                tooltip.push(json!({"text": format!("Линия: {group}")}));
            }
            tooltip.push(json!({"text": format!(
                "Отрезок: {} → {}",
                coordinate_text(&previous),
                coordinate_text(&current)
            )}));
            if let (Some(previous_sort), Some(current_sort)) =
                (sort_text(previous_row), sort_text(window[1].1))
            {
                tooltip.push(json!({"text": format!("Порядок: {previous_sort} → {current_sort}")}));
            }
            if let Some(text) = color_text(previous_row) {
                let mut entry = Map::new();
                if let Some(color) = &color {
                    entry.insert("color".to_string(), json!(color));
                }
                entry.insert("text".to_string(), json!(format!("Цвет: {text}")));
                tooltip.push(Value::Object(entry));
            }
            let previous_measures = measure_values(previous_row);
            let current_measures = measure_values(window[1].1);
            for (index, measure) in measures.iter().enumerate() {
                let previous_value = previous_measures.get(index).cloned().unwrap_or_default();
                let current_value = current_measures.get(index).cloned().unwrap_or_default();
                tooltip.push(json!({"text": format!(
                    "{}: {previous_value} → {current_value}",
                    polyline_field_title(measure, data)
                )}));
            }
            let mut options = Map::new();
            options.insert("strokeWidth".to_string(), json!(6));
            if let Some(color) = color {
                options.insert("strokeColor".to_string(), json!(color));
            }
            options.insert("zIndex".to_string(), json!(GEO_Z_INDEX_POLYLINE));
            children.push(json!({
                "feature": {
                    "geometry": {"type": "LineString", "coordinates": [previous, current]},
                    "properties": {"data": tooltip},
                },
                "options": Value::Object(options),
            }));
        }
        if show_points {
            for (pair, row) in &points {
                let mut tooltip = vec![json!({"text": format!("Точка: {}", coordinate_text(pair))})];
                if group != "DEFAULT_GROUP" {
                    tooltip.push(json!({"text": format!("Линия: {group}")}));
                }
                if let Some(text) = color_text(row) {
                    let mut entry = Map::new();
                    if let Some(color) = color_of(row) {
                        entry.insert("color".to_string(), json!(color));
                    }
                    entry.insert("text".to_string(), json!(format!("Цвет: {text}")));
                    tooltip.push(Value::Object(entry));
                }
                tooltip.extend(measure_text(row).into_iter().map(|text| json!({"text": text})));
                let mut options = Map::new();
                options.insert("preset".to_string(), json!("chartkit#chips"));
                if let Some(color) = color_of(row) {
                    options.insert("iconColor".to_string(), json!(color));
                }
                children.push(json!({
                    "feature": {
                        "geometry": {"type": "Point", "coordinates": pair},
                        "properties": {"radius": 1.5, "data": tooltip},
                    },
                    "options": Value::Object(options),
                }));
            }
        }
    }
    let mut options = Map::new();
    options.insert("opacity".to_string(), number(alpha));
    if let Some(id) = layer
        .pointer("/layerSettings/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        options.insert("geoObjectId".to_string(), json!(id));
    }
    let mut value = Map::new();
    value.insert("collection".to_string(), json!({"children": children}));
    value.insert("options".to_string(), Value::Object(options));
    if bounds_enabled(config) {
        value.insert("bounds".to_string(), bounds.as_value());
    }
    LayerOutput {
        value: Value::Object(value),
        bounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(fields: &[(&str, &str, &str)], rows: &[Vec<Value>]) -> Value {
        let fields: Vec<Value> = fields
            .iter()
            .enumerate()
            .map(|(index, (id, title, data_type))| {
                json!({
                    "legend_item_id": index,
                    "id": id,
                    "title": title,
                    "data_type": data_type,
                })
            })
            .collect();
        let legend: Vec<i64> = (0..fields.len() as i64).collect();
        let rows: Vec<Value> = rows
            .iter()
            .map(|values| json!({"data": values, "legend": legend}))
            .collect();
        json!({
            "fields": fields,
            "result_data": [{"rows": rows}],
            "blocks": [{"query": "select 1"}],
        })
    }

    fn prepared(config: Value) -> GraphPrepared {
        GraphPrepared {
            config,
            params: json!({}),
            requests: vec![graph_engine::GraphSourceRequest {
                key: "qvnkqz8s0wdqf_resultlayer".to_string(),
                dataset: "qvnkqz8s0wdqf".to_string(),
                body: json!({}),
            }],
            drill_down: Value::Null,
        }
    }

    fn fetched(body: Value) -> Vec<GraphFetched> {
        vec![GraphFetched {
            key: "qvnkqz8s0wdqf_resultlayer".to_string(),
            dataset: "qvnkqz8s0wdqf".to_string(),
            status: 200,
            latency: 0.0,
            size: 1,
            body,
            data_export: None,
        }]
    }

    fn test_config(layer_id: &str, color: Option<Value>) -> Value {
        json!({
            "extraSettings": {},
            "datasetsPartialFields": [],
            "visualization": {
                "id": "geolayer",
                "layers": [{
                    "id": layer_id,
                    "layerSettings": {"type": layer_id, "id": "layer-1", "name": "Layer 1", "alpha": 100},
                    "placeholders": [],
                    "commonPlaceholders": {
                        "colors": color.into_iter().collect::<Vec<_>>(),
                        "colorsConfig": {"gradientMode": "3-point", "gradientPalette": "orange-gray-blue"},
                        "geopointsConfig": {"radius": 4, "minRadius": 1, "maxRadius": 3},
                        "tooltips": [],
                        "labels": [],
                        "sort": []
                    }
                }]
            }
        })
    }

    #[test]
    fn polygons_keep_coordinates_and_gradient_options() {
        let polygon = "[[[32.0, 34.0], [32.1, 34.1], [32.0, 34.0]]]";
        let mut config = test_config(
            "geopolygon",
            Some(
                json!({"guid": "color", "title": "Delivered, %", "data_type": "float", "type": "MEASURE"}),
            ),
        );
        config["visualization"]["layers"][0]["placeholders"] = json!([{
            "id": "geopolygon",
            "items": [{"guid": "poly", "title": "Polygon", "data_type": "geopolygon"}],
        }]);
        let body = response(
            &[
                ("poly", "Polygon", "geopolygon"),
                ("color", "Delivered, %", "float"),
            ],
            &[
                vec![json!(polygon), json!(0.2)],
                vec![json!(polygon), json!(0.8)],
            ],
        );
        let (data, bounds) = build_data(&prepared(config), &fetched(body)).expect("geo data");
        let layer = &data[0];
        let features = layer
            .pointer("/polygonmap/polygons/features")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(features.len(), 2, "one feature per polygon row");
        assert_eq!(features[0]["geometry"]["type"], "Polygon");
        assert_eq!(
            features[0]["geometry"]["coordinates"][0][0],
            json!([32.0, 34.0])
        );
        assert!(features[0]["options"]["fillColor"].is_string());
        assert_eq!(layer["options"]["mode"], "gradient");
        assert_eq!(layer["options"]["gradientData"]["id"], "orange-gray-blue");
        assert_eq!(bounds.left_lat, Some(32.0));
        assert_eq!(bounds.right_long, Some(34.1));
    }

    #[test]
    fn geopoint_cluster_contains_points_and_bounds() {
        let mut config = test_config("geopoint-with-cluster", None);
        config["visualization"]["layers"][0]["placeholders"] = json!([
            {"id": "geopoint", "items": [{"guid": "point", "title": "Points", "data_type": "geopoint"}]},
            {"id": "size", "items": [{"guid": "size", "title": "Clients", "data_type": "integer", "type": "MEASURE"}]},
        ]);
        let body = response(
            &[
                ("point", "Points", "geopoint"),
                ("size", "Clients", "integer"),
            ],
            &[
                vec![json!("[32.1, 34.8]"), json!(10)],
                vec![json!("[32.2, 34.9]"), json!(20)],
            ],
        );
        let (data, _) = build_data(&prepared(config), &fetched(body)).expect("geo data");
        let layer = &data[0];
        let clusterer = layer["clusterer"].as_array().unwrap();
        assert_eq!(clusterer.len(), 2);
        assert_eq!(clusterer[0]["feature"]["geometry"]["type"], "Point");
        assert_eq!(
            clusterer[0]["feature"]["geometry"]["coordinates"],
            json!([32.1, 34.8])
        );
        assert_eq!(layer["options"]["iconPieChartRadius"], 20);
        assert_eq!(layer["options"]["zIndex"], Value::Null);
    }

    #[test]
    fn heatmap_weights_points_and_uses_default_gradient() {
        let mut config = test_config("heatmap", None);
        config["visualization"]["layers"][0]["placeholders"] = json!([
            {"id": "heatmap", "items": [{"guid": "heat", "title": "Points", "data_type": "geopoint"}]},
        ]);
        let body = response(
            &[("heat", "Points", "geopoint")],
            &[vec![json!("[32.0, 34.0]")], vec![json!("[32.1, 34.1]")]],
        );
        let (data, _) = build_data(&prepared(config), &fetched(body)).expect("geo data");
        let layer = &data[0];
        assert_eq!(layer["heatmap"].as_array().unwrap().len(), 2);
        assert_eq!(layer["heatmap"][0]["properties"]["weight"], 5);
        assert_eq!(layer["options"]["radius"], 15);
        assert_eq!(
            layer["options"]["gradient"]["0.1"],
            "rgba(128, 255, 0, 0.7)"
        );
    }

    #[test]
    fn polyline_colours_tooltips_and_sorted_dates() {
        let mut config = test_config(
            "polyline",
            Some(json!({"guid": "color", "title": "Zone", "data_type": "string", "type": "DIMENSION"})),
        );
        config["visualization"]["layers"][0]["placeholders"] = json!([
            {
                "id": "polyline",
                "items": [{"guid": "line", "title": "Line", "data_type": "geopoint"}],
                "settings": {"polylinePoints": "on"}
            },
            {"id": "grouping", "items": [{"guid": "group", "title": "Route", "data_type": "string"}]},
            {"id": "measures", "items": [{"guid": "measure", "title": "Cars", "data_type": "integer"}]},
        ]);
        config["visualization"]["layers"][0]["commonPlaceholders"]["sort"] = json!([{
            "guid": "sort", "title": "Day", "data_type": "date", "format": "DD.MM.YYYY"
        }]);
        let body = response(
            &[
                ("line", "Line", "geopoint"),
                ("group", "Route", "string"),
                ("measure", "Cars", "integer"),
                ("color", "Zone", "string"),
                ("sort", "Day", "date"),
            ],
            &[
                vec![json!("[32.0, 34.0]"), json!("A"), json!(5), json!("A"), json!("2020-02-01")],
                vec![json!("[33.0, 35.0]"), json!("A"), json!(7), json!("A"), json!("2020-02-02")],
                vec![json!("[34.0, 36.0]"), json!("A"), json!(9), json!("A"), json!("2020-02-03")],
            ],
        );
        let (data, _) = build_data(&prepared(config), &fetched(body)).expect("polyline data");
        let children = data
            .pointer("/0/collection/children")
            .and_then(Value::as_array)
            .expect("children");
        assert_eq!(children.len(), 5, "2 segments + 3 points");
        let segment = &children[0];
        assert_eq!(segment["options"]["strokeWidth"], json!(6));
        assert!(segment["options"]["strokeColor"].is_string());
        let texts: Vec<&str> = segment["feature"]["properties"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["text"].as_str())
            .collect();
        for expected in [
            "Линия: Route: A",
            "Отрезок: 32,34 → 33,35",
            "Порядок: 01.02.2020 → 02.02.2020",
            "Цвет: Zone: A",
            "Cars: 5 → 7",
        ] {
            assert!(texts.contains(&expected), "missing {expected:?} in {texts:?}");
        }
        let point = &children[2];
        assert_eq!(point["feature"]["geometry"]["type"], "Point");
        assert_eq!(point["feature"]["properties"]["radius"], json!(1.5));
        assert!(point["options"]["iconColor"].is_string());
        assert_eq!(
            point["feature"]["properties"]["data"][0]["text"],
            "Точка: 32,34"
        );
    }

    #[test]
    fn non_json_geometry_is_skipped_not_fatal() {
        let mut config = test_config("geopolygon", None);
        config["visualization"]["layers"][0]["placeholders"] = json!([
            {"id": "geopolygon", "items": [{"guid": "poly", "title": "Polygon", "data_type": "geopolygon"}]},
        ]);
        let body = response(
            &[("poly", "Polygon", "geopolygon")],
            &[vec![json!("Afeka")]],
        );
        let (data, bounds) = build_data(&prepared(config), &fetched(body)).expect("geo data");
        assert_eq!(data[0]["polygonmap"]["polygons"]["features"], json!([]));
        assert_eq!(data[0]["bounds"], json!([null, null]));
        assert_eq!(bounds.left_lat, None);
    }
}
