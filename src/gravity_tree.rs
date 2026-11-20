//! Rust port of the **Gravity treemap** visualization (`graph_wizard_node` with
//! `visualization.id == "treemap"`).
//!
//! Reference implementation: `bundle/worker.sync.js`
//!
//! * `preparers/treemap/gravity-charts.js:prepareD3Treemap` – treemap nodes (ids, parent ids,
//!   names, drill-down values), palette colours, levels, export settings;
//! * `gravity-charts/utils/index.js:getBaseChartConfig` – the shared gravity chart shell;
//! * `gravity-charts/utils/format.js` / `utils/misc-helpers.js` – number labels;
//! * `config/index.js:buildChartsConfigPrivate` – the `config` member (no treemap-specific keys);
//! * `url/build-sources.js` + `url/build-request-body/*` – the data-api request (delegated to
//!   [`crate::graph_engine::prepare`], which already mirrors them for graph nodes).
//!
//! `highchartsConfig` is `"{}"` for every Gravity chart (`buildLibraryConfig` returns `{}`).
//!
//! The module mirrors the sibling focused ports (`gravity_bar`, `gravity_scatter`,
//! `gravity_donut`): [`prepare`] returns the shared [`crate::graph_engine::GraphPrepared`] and
//! [`finish`] returns [`crate::graph_engine::GraphFinished`], so `run_rust_graph` keeps one
//! envelope path. Wiring (see `patches/variants_b.md`):
//!
//! ```ignore
//! // main.rs
//! mod gravity_tree;
//! // pipeline.rs -> run_rust_graph
//! "treemap" => crate::gravity_tree::prepare(shared, params, &datasets),
//! "treemap" => crate::gravity_tree::finish(&prepared, &fetched, &state.default_color_palette),
//! ```
//!
//! Ported scope: 1–3 string dimensions, exactly one numeric measure, at most one colour field
//! (string dimension, no gradient/mounted-colour branch is rejected), no markup/html/markdown
//! fields, no labels/tooltips/shapes/segments/links/chart filters, one dataset, no layers.
//! Everything else returns `Err` so the caller can fall back to V8.

use crate::graph_engine::{GraphFetched, GraphFinished, GraphPrepared};
use crate::gravity_donut::{
    array, data_type, export_column_settings, field_id, find_index_in_order, get_base_chart_config,
    get_color, get_colors_settings, get_format_options, get_mounted_color, is_dimension_field,
    is_number_field, js_number_from_f64, js_number_to_f64, localize_data,
    merge_json, merge_single_dataset, placeholder_items, s, truthy, with_data_type, MergedData,
};
use serde_json::{json, Map, Value};

const MAX_PADDING: i64 = 5;

fn is_string_field(field: &Value) -> bool {
    data_type(field) == "string"
}

fn is_markup_ish(field: &Value) -> bool {
    if data_type(field) != "string" {
        return false;
    }
    !s(field, "markupType").is_empty() || !s(field, "markup_type").is_empty()
}

/// Cheap dispatch guard used by `run_rust_graph`.
pub fn supports(shared: &Value) -> bool {
    let visualization = shared.get("visualization").cloned().unwrap_or_else(|| json!({}));
    if s(&visualization, "id") != "treemap" {
        return false;
    }
    if !array(&visualization, "layers").is_empty() {
        return false;
    }
    if array(shared, "datasetsIds").len() != 1 {
        return false;
    }
    let placeholders = array(&visualization, "placeholders");
    let dimensions = placeholder_items(placeholders, "dimensions");
    let measures = placeholder_items(placeholders, "measures");
    if dimensions.is_empty() || dimensions.len() > 3 || measures.len() != 1 {
        return false;
    }
    if !dimensions.iter().all(is_string_field) || !is_number_field(&measures[0]) {
        return false;
    }
    if dimensions.iter().any(is_markup_ish) {
        return false;
    }
    if !array(shared, "labels").is_empty() {
        return false;
    }
    for unsupported in ["shapes", "segments", "tooltips", "links", "filters"] {
        if !array(shared, unsupported).is_empty() {
            return false;
        }
    }
    if let Some(color) = array(shared, "colors").first() {
        if !is_dimension_field(color) || !is_string_field(color) || is_markup_ish(color) {
            return false;
        }
    }
    let colors_config = shared.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    if colors_config.get("colorMode").is_some() {
        return false;
    }
    true
}

/// Validate the migrated config against the treemap feature set (v9: structural checks
/// only; extra visual settings are normalised rather than rejected).
pub fn check_supported(config: &Value) -> Result<(), String> {
    let visualization = config
        .get("visualization")
        .cloned()
        .ok_or_else(|| "gravity tree: visualization is missing".to_string())?;
    if s(&visualization, "id") != "treemap" {
        return Err("gravity tree: visualization is not treemap".to_string());
    }
    Ok(())
}

/// Normalise visual settings the treemap builder does not render.
fn sanitize_config(config: &mut Value) {
    if let Some(visualization) = config.get_mut("visualization") {
        if let Some(layers) = visualization.get("layers").and_then(Value::as_array).cloned() {
            if let Some(first) = layers.first() {
                if let Some(placeholders) = first.get("placeholders").cloned() {
                    visualization["placeholders"] = placeholders;
                }
            }
            visualization["layers"] = json!([]);
        }
        let placeholders = array(visualization, "placeholders").to_vec();
        let mut normalized: Vec<Value> = Vec::new();
        for placeholder in &placeholders {
            let id = s(placeholder, "id");
            let keep = match id {
                "dimensions" => 3,
                "measures" => 1,
                _ => array(placeholder, "items").len(),
            };
            let mut placeholder = placeholder.clone();
            let items: Vec<Value> = array(&placeholder, "items")
                .iter()
                .filter(|item| !is_markup_ish(item))
                .take(keep)
                .cloned()
                .collect();
            placeholder["items"] = Value::Array(items);
            normalized.push(placeholder);
        }
        visualization["placeholders"] = Value::Array(normalized);
    }
    config["labels"] = json!([]);
    config["sort"] = json!([]);
    config["shapes"] = json!([]);
    config["segments"] = json!([]);
    config["tooltips"] = json!([]);
    config["links"] = json!([]);
    config["filters"] = json!([]);
    if let Some(map) = config.as_object_mut() {
        map.remove("tooltipConfig");
    }
    if let Some(settings) = config.get_mut("colorsConfig").and_then(Value::as_object_mut) {
        settings.remove("colorMode");
        settings.remove("gradientMode");
        if let Some(palette) = settings.get("palette").and_then(Value::as_str) {
            if !crate::gravity_donut::palette_is_system(palette) {
                settings.remove("palette");
            }
        }
    }
    let colors: Vec<Value> = array(config, "colors")
        .iter()
        .filter(|color| is_dimension_field(color) && is_string_field(color) && !is_markup_ish(color))
        .take(1)
        .cloned()
        .collect();
    config["colors"] = Value::Array(colors);
}

/// Build the migrated config + data-api request through the shared graph request builder.
pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
) -> Result<GraphPrepared, String> {
    let mut prepared = crate::graph_engine::prepare(shared, params, datasets)?;
    check_supported(&prepared.config)?;
    sanitize_config(&mut prepared.config);
    if prepared.requests.len() != 1 {
        return Err("gravity tree: exactly one dataset is expected".to_string());
    }
    Ok(prepared)
}

fn chartkit_format_number(value: f64, formatting: &Value) -> String {
    crate::gravity_donut::format_number(value, formatting, "ru")
}

fn treemap_label(measure: &Value, value: &Value) -> String {
    let mut options = Map::new();
    options.insert("lang".to_string(), json!("ru"));
    if let Some(formatting) = get_format_options(measure).as_object() {
        for (key, item) in formatting {
            options.insert(key.clone(), item.clone());
        }
    }
    let number = js_number_to_f64(value);
    chartkit_format_number(number, &Value::Object(options))
}

fn row_color_value(row: &[Value], color_index: i64) -> Option<Value> {
    if color_index < 0 {
        return None;
    }
    row.get(color_index as usize).cloned()
}

/// Build the Gravity treemap `data` member (`prepareD3Treemap`).
pub fn build_treemap_data(
    config: &Value,
    merged: &MergedData,
    lang: &str,
    default_palette_id: &str,
) -> Result<Value, String> {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let placeholders = array(&visualization, "placeholders").to_vec();
    let dimensions: Vec<Value> = placeholder_items(&placeholders, "dimensions")
        .iter()
        .map(|field| with_data_type(field, &merged.id_to_data_type))
        .collect();
    let measure = placeholder_items(&placeholders, "measures")
        .first()
        .map(|field| with_data_type(field, &merged.id_to_data_type))
        .ok_or_else(|| "gravity tree: measure is missing".to_string())?;
    let color_field = array(config, "colors")
        .first()
        .map(|field| with_data_type(field, &merged.id_to_data_type));
    let measure_guid = field_id(&measure);
    let measure_title = merged
        .id_to_title
        .get(&measure_guid)
        .cloned()
        .unwrap_or_else(|| s(&measure, "title").to_string());
    let measure_index = find_index_in_order(&merged.order, &measure, &measure_title);
    if measure_index < 0 {
        return Err("gravity tree: measure is not in the response".to_string());
    }
    let color_index = color_field
        .as_ref()
        .map(|field| {
            let title = merged
                .id_to_title
                .get(&field_id(field))
                .cloned()
                .unwrap_or_else(|| s(field, "title").to_string());
            find_index_in_order(&merged.order, field, &title)
        })
        .unwrap_or(-1);
    let dimension_indexes: Vec<i64> = dimensions
        .iter()
        .map(|field| {
            let title = merged
                .id_to_title
                .get(&field_id(field))
                .cloned()
                .unwrap_or_else(|| s(field, "title").to_string());
            find_index_in_order(&merged.order, field, &title)
        })
        .collect();
    if dimension_indexes.iter().any(|index| *index < 0) {
        return Err("gravity tree: dimension is not in the response".to_string());
    }

    let mut treemap: Vec<Value> = Vec::new();
    let mut treemap_ids: Vec<String> = Vec::new();
    let mut hash: Map<String, Value> = Map::new();
    let mut values_for_color: Map<String, Value> = Map::new();
    for row in &merged.data {
        let color_by_dimension = row_color_value(row, color_index);
        let mut path: Vec<String> = Vec::new();
        let mut last_item: Option<Value> = None;
        for (level, _dimension) in dimensions.iter().enumerate() {
            let index = dimension_indexes[level] as usize;
            let Some(raw) = row.get(index) else { continue };
            let value = match raw {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let parent_id = if path.is_empty() {
                None
            } else {
                Some(format!("id_{}", path.join("/")))
            };
            path.push(value.clone());
            let mut item = Map::new();
            item.insert("id".to_string(), json!(format!("id_{}", path.join("/"))));
            item.insert("name".to_string(), json!(value));
            item.insert("drillDownFilterValue".to_string(), json!(value));
            if let Some(parent_id) = parent_id {
                item.insert("parentId".to_string(), json!(parent_id));
            }
            let item = Value::Object(item);
            if level == dimensions.len() - 1 {
                last_item = Some(item);
            } else {
                let id = s(&item, "id").to_string();
                if !treemap_ids.contains(&id) {
                    treemap_ids.push(id);
                    treemap.push(item);
                }
            }
        }
        let key = format!("id_{}", path.join("/"));
        let Some(value) = row.get(measure_index as usize) else { continue };
        hash.insert(
            key.clone(),
            json!({"value": value.clone(), "label": treemap_label(&measure, value)}),
        );
        if let Some(color_value) = &color_by_dimension {
            values_for_color.insert(key.clone(), color_value.clone());
        }
        if let Some(mut item) = last_item {
            let entry = hash.get(&key).cloned().unwrap_or_else(|| json!({}));
            let value = entry.get("value").cloned().unwrap_or(Value::Null);
            let label = entry.get("label").cloned().unwrap_or(Value::Null);
            if let Some(object) = item.as_object_mut() {
                object.insert("value".to_string(), js_number_from_f64(js_number_to_f64(&value)));
                object.insert("label".to_string(), label);
                object.insert(
                    "name".to_string(),
                    Value::Array(path.iter().map(|part| json!(part)).collect()),
                );
            }
            treemap.push(item);
        }
    }

    if let Some(color) = &color_field {
        let colors_config = config.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
        let (mounted, palette) = get_colors_settings(&colors_config, Some(color), default_palette_id);
        let mut known_values: Vec<String> = Vec::new();
        let mut color_data: Map<String, Value> = Map::new();
        for (key, color_value) in &values_for_color {
            let text = match color_value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let index = match known_values.iter().position(|known| known == &text) {
                Some(index) => index,
                None => {
                    known_values.push(text.clone());
                    known_values.len() - 1
                }
            };
            let color = if truthy(color_value) {
                get_mounted_color(&mounted, &palette, &text)
                    .unwrap_or_else(|| get_color(index, &palette))
            } else {
                get_color(index, &palette)
            };
            color_data.insert(key.clone(), json!({"backgroundColor": color}));
        }
        for item in treemap.iter_mut() {
            let id = s(item, "id").to_string();
            if let Some(entry) = color_data.get(&id) {
                if let Some(background) = entry.get("backgroundColor") {
                    if let Some(object) = item.as_object_mut() {
                        object.insert("color".to_string(), background.clone());
                    }
                }
            }
        }
    }

    let dimension_count = dimensions.len() as i64;
    let levels: Vec<Value> = (0..dimension_count)
        .map(|index| {
            json!({
                "index": index + 1,
                "padding": std::cmp::min(MAX_PADDING, (dimension_count - index) * 2 - 1)
            })
        })
        .collect();
    let export_columns: Vec<Value> = dimensions
        .iter()
        .enumerate()
        .map(|(index, field)| export_column_settings(&format!("name.{index}"), field))
        .chain(std::iter::once(export_column_settings("value", &measure)))
        .collect();
    let series = json!({
        "type": "treemap",
        "name": "",
        "layoutAlgorithm": "squarify",
        "dataLabels": {"enabled": true, "html": false},
        "levels": Value::Array(levels),
        "data": Value::Array(treemap),
        "sorting": {"enabled": true},
        "custom": {"exportSettings": {"columns": Value::Array(export_columns)}}
    });
    let mut data = get_base_chart_config(config);
    let override_value = json!({
        "series": {"data": [series]},
        "legend": {"enabled": false},
        "chart": {"zoom": {"enabled": false}}
    });
    merge_json(&mut data, override_value);
    localize_data(&mut data, lang);
    Ok(data)
}

/// `config` member for the treemap (`buildChartsConfigPrivate` has no treemap-specific keys).
pub(crate) fn build_config(config: &Value, params: &Value) -> Value {
    crate::gravity_donut::build_config(config, params)
}

/// Build the Gravity treemap `data`/`config`/`sources` for one data-api response.
pub fn finish(
    prepared: &GraphPrepared,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<GraphFinished, String> {
    finish_with_lang(prepared, fetched, default_palette_id, "en")
}

/// [`finish`] with the request user language (used by `finishRunRaw`'s `localize`).
pub fn finish_with_lang(
    prepared: &GraphPrepared,
    fetched: &[GraphFetched],
    default_palette_id: &str,
    lang: &str,
) -> Result<GraphFinished, String> {
    let response = fetched
        .first()
        .ok_or_else(|| "gravity tree: no fetched source".to_string())?;
    let dataset = prepared
        .requests
        .first()
        .map(|request| request.dataset.clone())
        .ok_or_else(|| "gravity tree: no prepared request".to_string())?;
    let merged = merge_single_dataset(&dataset, &response.body)?;
    let data = build_treemap_data(&prepared.config, &merged, lang, default_palette_id)?;
    let config = build_config(&prepared.config, &prepared.params);
    let mut sources = Map::new();
    for (request, response) in prepared.requests.iter().zip(fetched.iter()) {
        let mut source = Map::new();
        source.insert("sourceId".to_string(), json!(request.key));
        source.insert("sourceType".to_string(), json!("bi_datasets"));
        source.insert("status".to_string(), json!(response.status));
        source.insert("latency".to_string(), json!(response.latency));
        source.insert("size".to_string(), json!(response.size));
        source.insert("datasetId".to_string(), json!(request.dataset));
        source.insert("data".to_string(), request.body.clone());
        sources.insert(request.key.clone(), Value::Object(source));
    }
    Ok(GraphFinished {
        data: crate::graph_engine::GraphData::Value(data),
        config,
        library: json!({}),
        sources: Value::Object(sources),
    })
}

/// Convenience entry point (offline parity harness / tests), mirroring the other gravity ports.
pub fn run(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
    fetched: &[(String, Value, u16, f64, u64, Option<Value>)],
    default_palette_id: &str,
) -> Result<(GraphPrepared, GraphFinished), String> {
    let prepared = prepare(shared, params, datasets)?;
    let mut responses = Vec::with_capacity(fetched.len());
    for (key, body, status, latency, size, data_export) in fetched {
        responses.push(GraphFetched {
            key: key.clone(),
            dataset: prepared
                .requests
                .iter()
                .find(|request| &request.key == key)
                .map(|request| request.dataset.clone())
                .unwrap_or_default(),
            status: *status,
            latency: *latency,
            size: *size,
            body: body.clone(),
            data_export: data_export.clone(),
        });
    }
    let finished = finish_with_lang(&prepared, &responses, default_palette_id, "en")?;
    Ok((prepared, finished))
}
