//! Rust port of the DataLens chartkit `area` graph slice **for charts whose colour placeholder is
//! a wizard hierarchy** (the last `d3_wizard_node` entry, `27z4q4cl8umqr`).
//!
//! The chart is a Highcharts `graph_wizard_node` response (`visualization.id = "area"`), but US
//! stores the entry as `d3_wizard_node`, so it reaches `run_rust_graph` through the
//! `d3_wizard_node` dispatch arm. The wizard's `buildGraph` calls `preprocessHierarchies`
//! (`ui-dist/server/modes/charts/plugins/datalens/utils/hierarchy-helpers.js`) before building the
//! sources and `prepareSingleResult`; that helper:
//!
//! * replaces the hierarchy pseudo-field in its placeholder/`colors`/`shapes`/`segments`
//!   container with `hierarchy.fields[min(drillDownLevel, fields.length - 1)]`;
//! * stores `sharedData.drillDownData = {breadcrumbs, filters, level, fields, isColorDrillDown}`;
//! * `js/js.js` then turns that into the `userConfigOverride` `drillDown = {breadcrumbs,
//!   dateFormat}` (`dateFormat` = `getServerDateFormat(currentField.data_type)`) and into
//!   `userParamsOverride {drillDownLevel, drillDownFilters, isColorDrillDown}`.
//!
//! Without this preprocessing the hierarchy guid itself is sent to data-api and the request fails
//! with `FIELD.NOT_FOUND <hierarchy guid>`.
//!
//! Everything else is delegated to the existing `graph_engine` (it already builds the request,
//! `data.graphs`, `config` and `highchartsConfig` for colour-dimension `area` charts). This module
//! is deliberately narrow: one hierarchy, no layered visualization, no shapes/segments/labels or an
//! explicit sort; anything else returns `Err`, so `run_rust_graph` keeps the V8 fallback.

use crate::graph_engine::{self, DatasetInfo, GraphFetched, GraphFinished, GraphPrepared};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// small JSON helpers
// ---------------------------------------------------------------------------------------------

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn items_of(placeholder: &Value) -> &[Value] {
    placeholder.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn is_hierarchy_field(field: &Value) -> bool {
    s(field, "data_type") == "hierarchy"
}

fn is_parameter(field: &Value) -> bool {
    s(field, "calc_mode") == "parameter"
}

fn is_dimension_field(field: &Value) -> bool {
    s(field, "type") == "DIMENSION" && !is_parameter(field)
}

fn field_title(field: &Value) -> String {
    let fake = s(field, "fakeTitle");
    if !fake.is_empty() {
        fake.to_string()
    } else {
        s(field, "title").to_string()
    }
}

fn parameter_values(params: &Value, name: &str) -> Vec<Value> {
    match params.get(name) {
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// public gate
// ---------------------------------------------------------------------------------------------

/// `true` when any placeholder/`colors`/`shapes`/`segments` field is a hierarchy pseudo-field.
/// The pipeline only routes to this module when the gate matches, so charts that already work
/// through `graph_engine` are untouched.
pub fn handles(shared: &Value) -> bool {
    let mut containers: Vec<&[Value]> = Vec::new();
    if let Some(visualization) = shared.get("visualization") {
        for placeholder in array(visualization, "placeholders") {
            containers.push(items_of(placeholder));
        }
        for layer in array(visualization, "layers") {
            for placeholder in array(layer, "placeholders") {
                containers.push(items_of(placeholder));
            }
            if let Some(common) = layer.get("commonPlaceholders") {
                for key in ["colors", "shapes", "segments"] {
                    if let Some(items) = common.get(key).and_then(Value::as_array) {
                        containers.push(items);
                    }
                }
            }
        }
    }
    for key in ["colors", "shapes", "segments"] {
        if let Some(items) = shared.get(key).and_then(Value::as_array) {
            containers.push(items);
        }
    }
    containers
        .iter()
        .any(|items| items.iter().any(is_hierarchy_field))
}

// ---------------------------------------------------------------------------------------------
// `preprocessHierarchies` (`utils/hierarchy-helpers.js`)
// ---------------------------------------------------------------------------------------------

struct HierarchyState {
    drill_down: Option<(Vec<Value>, Vec<Value>, bool)>, // fields, filters, is_color_drill_down
    count: usize,
    level: usize,
}

impl HierarchyState {
    fn new(level: usize) -> Self {
        Self {
            drill_down: None,
            count: 0,
            level,
        }
    }

    fn note_container(&mut self, container_id: &str) {
        if matches!(container_id, "colors" | "shapes") {
            if let Some((_, _, flag)) = self.drill_down.as_mut() {
                *flag = true;
            }
        }
    }
}

fn expand_hierarchy(
    items: &mut [Value],
    container_id: &str,
    state: &mut HierarchyState,
    params: &Value,
) -> Result<(), String> {
    for item in items.iter_mut() {
        if !is_hierarchy_field(item) {
            continue;
        }
        state.count += 1;
        if state.count > 1 {
            // The JS helper supports several hierarchies; the chart in scope has exactly one.
            return Err("d3 area: more than one hierarchy is not ported".to_string());
        }
        let mut fields: Vec<Value> = item
            .get("fields")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // `removeMarkupFieldsFromHierarchy`
        fields.retain(|field| s(field, "data_type") != "markup");
        if fields.is_empty() {
            return Err("d3 area: hierarchy has no fields".to_string());
        }
        let filters = match params.get("drillDownFilters") {
            Some(Value::Array(filters)) => filters.clone(),
            _ => fields.iter().map(|_| json!("")).collect(),
        };
        let is_color_drill_down = matches!(container_id, "colors" | "shapes");
        let field_index = state.level.min(fields.len() - 1);
        *item = fields[field_index].clone();
        state.drill_down = Some((fields, filters, is_color_drill_down));
        state.note_container(container_id);
    }
    Ok(())
}

fn drill_down_level(params: &Value) -> usize {
    // `getDrillDownLevel`: Number((params.drillDownLevel || ['0'])[0])
    parameter_values(params, "drillDownLevel")
        .first()
        .and_then(|value| match value {
            Value::String(text) => text.parse::<f64>().ok(),
            Value::Number(number) => number.as_f64(),
            _ => None,
        })
        .unwrap_or(0.0)
        .max(0.0) as usize
}

fn server_date_format(data_type: &str) -> &'static str {
    if matches!(data_type, "datetime" | "genericdatetime") {
        "YYYY-MM-DDTHH:mm:ss"
    } else {
        "YYYY-MM-DD"
    }
}

/// Expand every hierarchy in the migrated config in place and return
/// `(config, sharedData.drillDownData, userParamsOverride)`.
fn preprocess_hierarchies(
    config: &Value,
    params: &Value,
) -> Result<(Value, Option<Value>, Map<String, Value>), String> {
    let params = graph_engine::resolve_params(params);
    let level = drill_down_level(&params);
    let mut config = config.clone();
    let mut state = HierarchyState::new(level);
    if let Some(visualization) = config.get_mut("visualization") {
        if !array(visualization, "layers").is_empty() {
            return Err("d3 area: layered visualizations are not ported".to_string());
        }
        if let Some(placeholders) = visualization.get_mut("placeholders").and_then(Value::as_array_mut) {
            for placeholder in placeholders.iter_mut() {
                let placeholder_id = s(placeholder, "id").to_string();
                if let Some(items) = placeholder.get_mut("items").and_then(Value::as_array_mut) {
                    expand_hierarchy(items, &placeholder_id, &mut state, &params)?;
                }
            }
        }
    }
    for key in ["colors", "shapes", "segments"] {
        if let Some(items) = config.get_mut(key).and_then(Value::as_array_mut) {
            expand_hierarchy(items, key, &mut state, &params)?;
        }
    }
    let drill_down = match state.drill_down {
        Some((fields, filters, is_color_drill_down)) => {
            let breadcrumbs: Vec<Value> = fields.iter().map(|field| json!(field_title(field))).collect();
            Some(json!({
                "breadcrumbs": breadcrumbs,
                "filters": filters,
                "level": level,
                "fields": fields,
                "isColorDrillDown": is_color_drill_down,
            }))
        }
        None => None,
    };
    let mut params_extra = Map::new();
    if let Some(drill_down) = &drill_down {
        params_extra.insert("drillDownLevel".to_string(), json!(level));
        params_extra.insert(
            "drillDownFilters".to_string(),
            drill_down.get("filters").cloned().unwrap_or_else(|| json!([])),
        );
        params_extra.insert(
            "isColorDrillDown".to_string(),
            drill_down
                .get("isColorDrillDown")
                .cloned()
                .unwrap_or_else(|| json!(false)),
        );
    }
    Ok((config, drill_down, params_extra))
}

/// `drillDown` `userConfigOverride` from `sharedData.drillDownData` (`js/js.js:291-298`).
fn drill_down_override(drill_down_data: &Value) -> Option<Value> {
    let fields = drill_down_data.get("fields")?.as_array()?;
    let level = drill_down_data.get("level").and_then(Value::as_u64).unwrap_or(0) as usize;
    let current = fields.get(level.min(fields.len().saturating_sub(1)))?;
    Some(json!({
        "breadcrumbs": drill_down_data
            .get("breadcrumbs")
            .cloned()
            .unwrap_or_else(|| json!([])),
        "dateFormat": server_date_format(s(current, "data_type")),
    }))
}

// ---------------------------------------------------------------------------------------------
// prepare / finish
// ---------------------------------------------------------------------------------------------

/// Build the migrated config with expanded hierarchies, the data-api request and the wizard
/// `sharedData`/params overrides.
pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &HashMap<String, DatasetInfo>,
) -> Result<GraphPrepared, String> {
    let config = graph_engine::server_config(shared)?;
    let (expanded, drill_down_data, params_extra) = preprocess_hierarchies(&config, params)?;
    let mut prepared = graph_engine::prepare(&expanded, params, datasets)?;
    if let Some(drill_down_data) = drill_down_data {
        // Keep the wizard's `sharedData` shape; `graph_engine` ignores unknown config members,
        // and `finish` derives the `config.drillDown` override from it (like `js/js.js`).
        prepared.config["sharedData"] = json!({ "drillDownData": drill_down_data });
    }
    if let Some(object) = prepared.params.as_object_mut() {
        for (key, value) in params_extra {
            object.insert(key, value);
        }
    }
    Ok(prepared)
}

/// `utils/misc-helpers.js::collator` (`new Intl.Collator(undefined, {numeric: true,
/// sensitivity: 'base'})`) for the colour values used by `getSortedLineKeys` when no explicit sort
/// is configured.
fn collator_compare(left: &str, right: &str) -> Ordering {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let mut index = 0;
    loop {
        if index >= left_chars.len() && index >= right_chars.len() {
            return Ordering::Equal;
        }
        if index >= left_chars.len() {
            return Ordering::Less;
        }
        if index >= right_chars.len() {
            return Ordering::Greater;
        }
        let left_char = left_chars[index];
        let right_char = right_chars[index];
        if left_char.is_ascii_digit() && right_char.is_ascii_digit() {
            let mut left_end = index;
            while left_end < left_chars.len() && left_chars[left_end].is_ascii_digit() {
                left_end += 1;
            }
            let mut right_end = index;
            while right_end < right_chars.len() && right_chars[right_end].is_ascii_digit() {
                right_end += 1;
            }
            let left_number: String = left_chars[index..left_end].iter().collect();
            let right_number: String = right_chars[index..right_end].iter().collect();
            let left_trimmed = left_number.trim_start_matches('0');
            let right_trimmed = right_number.trim_start_matches('0');
            match left_trimmed.len().cmp(&right_trimmed.len()) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
            match left_trimmed.cmp(right_trimmed) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
            index = left_end.max(right_end);
            continue;
        }
        let left_folded = left_char.to_lowercase().next().unwrap_or(left_char);
        let right_folded = right_char.to_lowercase().next().unwrap_or(right_char);
        match left_folded.cmp(&right_folded) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
        index += 1;
    }
}

fn response_index_by_guid(response: &Value) -> HashMap<String, usize> {
    let legend: Vec<i64> = response
        .pointer("/result_data/0/rows/0/legend")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let mut index_by_guid = HashMap::new();
    if let Some(fields) = response.get("fields").and_then(Value::as_array) {
        for field in fields {
            let Some(legend_id) = field.get("legend_item_id").and_then(Value::as_i64) else {
                continue;
            };
            let Some(position) = legend.iter().position(|id| *id == legend_id) else {
                continue;
            };
            index_by_guid
                .entry(s(field, "id").to_string())
                .or_insert(position);
        }
    }
    index_by_guid
}

fn clone_with_body(response: &GraphFetched, body: Value) -> GraphFetched {
    GraphFetched {
        key: response.key.clone(),
        dataset: response.dataset.clone(),
        status: response.status,
        latency: response.latency,
        size: response.size,
        body,
        data_export: response.data_export.clone(),
    }
}

/// `getSortedLineKeys` sorts the colour line keys with `collator.compare` when no explicit sort is
/// configured. `graph_engine::build_data` keeps the first-appearance order (the data-api order), so
/// mirror the wizard by feeding rows ordered by the colour value; every point is keyed by category,
/// so this only changes the graph/colour order.
fn sort_fetched_by_color(
    config: &Value,
    fetched: &[GraphFetched],
) -> Result<Vec<GraphFetched>, String> {
    if !array(config, "sort").is_empty() {
        return Err("d3 area: explicit sort is not ported".to_string());
    }
    if !array(config, "shapes").is_empty() && !array(config, "shapes").iter().all(is_hierarchy_field) {
        return Err("d3 area: shapes are not ported".to_string());
    }
    if !array(config, "segments").is_empty() {
        return Err("d3 area: segments are not ported".to_string());
    }
    if !array(config, "labels").is_empty() {
        return Err("d3 area: labels are not ported".to_string());
    }
    let Some(color_field) = array(config, "colors")
        .iter()
        .find(|field| is_dimension_field(field))
    else {
        return Ok(fetched
            .iter()
            .map(|response| clone_with_body(response, response.body.clone()))
            .collect());
    };
    let color_guid = s(color_field, "guid").to_string();
    let mut sorted = Vec::with_capacity(fetched.len());
    for response in fetched {
        let mut body = response.body.clone();
        let index_by_guid = response_index_by_guid(&body);
        let Some(color_index) = index_by_guid.get(&color_guid).copied() else {
            return Err("d3 area: colour field is not in the response".to_string());
        };
        let Some(rows) = body
            .pointer_mut("/result_data/0/rows")
            .and_then(Value::as_array_mut)
        else {
            return Err("d3 area: response rows are missing".to_string());
        };
        rows.sort_by(|left, right| {
            let value_at = |row: &Value| {
                row.get("data")
                    .and_then(Value::as_array)
                    .and_then(|values| values.get(color_index))
                    .map(text_of)
                    .unwrap_or_default()
            };
            collator_compare(&value_at(left), &value_at(right))
        });
        sorted.push(clone_with_body(response, body));
    }
    Ok(sorted)
}

/// Finish the response: delegate to `graph_engine`, then add the `drillDown` config override.
pub fn finish(
    prepared: &GraphPrepared,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<GraphFinished, String> {
    let drill_down = prepared
        .config
        .pointer("/sharedData/drillDownData")
        .and_then(drill_down_override);
    let sorted = sort_fetched_by_color(&prepared.config, fetched)?;
    let mut finished = graph_engine::finish(prepared, &sorted, default_palette_id)?;
    if let Some(drill_down) = drill_down {
        if let Some(object) = finished.config.as_object_mut() {
            object.insert("drillDown".to_string(), drill_down);
        }
    }
    Ok(finished)
}
