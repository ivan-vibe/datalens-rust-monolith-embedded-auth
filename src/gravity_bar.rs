//! Rust port of the DataLens **Gravity Charts** `bar` renderer (wizard `d3_wizard_node`
//! response) for the chart slice used by the `?tab=7g` dashboard.
//!
//! JS reference:
//!
//! * `preparers/bar-y/prepare-bar-y-data.js` – `prepareBarYData` (categories, lines, sorting);
//! * `preparers/bar-y/gravity-charts.js` – `prepareGravityChartsBarY` (Gravity series transform,
//!   axes, legend, tooltip, exporting);
//! * `gravity-charts/utils/index.js` – `getBaseChartConfig`;
//! * `gravity-charts/utils/format.js` – `getFieldFormatOptions`;
//! * `preparers/utils.js` – `mapToGravityChartValueFormat`,
//!   `mapChartkitFormatSettingsToGravityChartValueFormat`;
//! * `utils/export-helpers.js` – `getExportColumnSettings`;
//! * `utils/color-helpers.js` + `server/constants/index.js` – palette/mounted-colour resolution;
//! * `config/index.js` – `buildChartsConfigPrivate` (the envelope `config`).
//!
//! The port is deliberately narrow: only the plain horizontal `bar` visualization without
//! shapes, segments or extra datasets is covered. Axis label formatting is taken from
//! `axisFormatMode` (`by-field`/`manual` → `xAxis.labels.numberFormat`), like
//! `getAxisFormatting`. Everything else returns `Err`; `run_rust_graph` then keeps the
//! existing V8 fallback, so unsupported charts stay byte-identical.

use crate::graph_engine::{self, DatasetInfo, GraphFetched};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------------
// small JSON helpers (mirroring the private ones in `graph_engine`)
// ---------------------------------------------------------------------------------------------

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn find_placeholder<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    placeholders.iter().find(|placeholder| s(placeholder, "id") == id)
}

fn is_number_field(field: &Value) -> bool {
    matches!(s(field, "data_type"), "integer" | "uinteger" | "float")
}

fn is_float_field(field: &Value) -> bool {
    s(field, "data_type") == "float"
}

fn is_date_field(field: &Value) -> bool {
    matches!(
        s(field, "data_type"),
        "date" | "datetime" | "genericdatetime" | "datetimetz"
    )
}

fn is_measure_field(field: &Value) -> bool {
    s(field, "type") == "MEASURE"
}

fn is_pseudo_field(field: &Value) -> bool {
    s(field, "type") == "PSEUDO"
}

fn markup_kind(field: &Value) -> &str {
    s(field, "markupType")
}

fn fake_title(field: &Value) -> String {
    let fake = s(field, "fakeTitle");
    if !fake.is_empty() {
        return fake.to_string();
    }
    s(field, "title").to_string()
}

fn original_or_title(field: &Value) -> String {
    let original = s(field, "originalTitle");
    if original.is_empty() {
        s(field, "title").to_string()
    } else {
        original.to_string()
    }
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

const MINIMUM_FRACTION_DIGITS: i64 = 2;

/// `getFormatOptions` (`shared/modules/formatters`).
fn get_format_options(field: &Value) -> Value {
    if !is_number_field(field) {
        return Value::Null;
    }
    let mut formatting = field.get("formatting").cloned().unwrap_or_else(|| json!({}));
    if formatting.as_object().map(Map::is_empty).unwrap_or(true) {
        formatting = serde_json::from_str(s(field, "ui_settings"))
            .ok()
            .and_then(|value: Value| value.pointer("/numberFormatting").cloned())
            .unwrap_or_else(|| json!({}));
    }
    if formatting.get("precision").and_then(Value::as_f64).is_none() && is_float_field(field) {
        formatting["precision"] = json!(MINIMUM_FRACTION_DIGITS);
    }
    formatting
}

/// `Some(formatting)` when the JS helper would take the "non-empty" branch.
fn non_empty_object(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) if !map.is_empty() => Some(value),
        _ => None,
    }
}

/// `getFormatOptionsFromFieldFormatting` (chartkit format settings).
fn chartkit_format(formatting: Option<&Value>, data_type: &str) -> Value {
    let precision = if data_type == "float" {
        formatting
            .and_then(|value| value.get("precision"))
            .and_then(Value::as_f64)
            .unwrap_or(MINIMUM_FRACTION_DIGITS as f64) as i64
    } else {
        0
    };
    let Some(formatting) = formatting else {
        return json!({"chartKitFormatting": true, "chartKitPrecision": precision});
    };
    let mut result = Map::new();
    result.insert("chartKitFormatting".to_string(), json!(true));
    result.insert("chartKitPrecision".to_string(), json!(precision));
    for (key, output) in [
        ("prefix", "chartKitPrefix"),
        ("postfix", "chartKitPostfix"),
        ("unit", "chartKitUnit"),
        ("format", "chartKitFormat"),
        ("labelMode", "chartKitLabelMode"),
        ("showRankDelimiter", "chartKitShowRankDelimiter"),
    ] {
        if let Some(value) = formatting.get(key) {
            if !value.is_null() {
                result.insert(output.to_string(), value.clone());
            }
        }
    }
    Value::Object(result)
}

/// `mapChartkitFormatSettingsToGravityChartValueFormat`.
fn map_chartkit_format(settings: &Value) -> Value {
    let mut result = Map::new();
    for (input, output) in [
        ("chartKitFormat", "format"),
        ("chartKitLabelMode", "labelMode"),
        ("chartKitPostfix", "postfix"),
        ("chartKitPrecision", "precision"),
        ("chartKitPrefix", "prefix"),
        ("chartKitShowRankDelimiter", "showRankDelimiter"),
    ] {
        if let Some(value) = settings.get(input) {
            if !value.is_null() {
                result.insert(output.to_string(), value.clone());
            }
        }
    }
    result.insert("type".to_string(), json!("number"));
    if let Some(unit) = settings.get("chartKitUnit") {
        result.insert("unit".to_string(), unit.clone());
    }
    Value::Object(result)
}

/// `getFieldFormatOptions` (`gravity-charts/utils/format.js`).
fn field_format_options(field: &Value) -> Value {
    if is_number_field(field) {
        let formatting = get_format_options(field);
        let label_format = if s(&formatting, "labelMode") == "percent" {
            "percent"
        } else {
            s(&formatting, "format")
        };
        let mut result = Map::new();
        result.insert("type".to_string(), json!("number"));
        if let Some(fields) = formatting.as_object() {
            for (key, value) in fields {
                result.insert(key.clone(), value.clone());
            }
        }
        // `JS`: `format: labelFormat` where labelFormat is `undefined` when the field has no
        // `format` key and labelMode is not "percent"; `undefined` keys are dropped by
        // `JSON.stringify`.
        if !label_format.is_empty() {
            result.insert("format".to_string(), json!(label_format));
        }
        return Value::Object(result);
    }
    if is_date_field(field) {
        return json!({"type": "date", "format": s(field, "format")});
    }
    Value::Null
}

/// `mapToGravityChartValueFormat`.
fn map_to_gravity_value_format(field: Option<&Value>, format_settings: Option<&Value>) -> Value {
    let Some(format_settings) = format_settings else {
        return Value::Null;
    };
    let is_number = field
        .map(|field| is_number_field(field) || is_measure_field(field))
        .unwrap_or(false);
    if is_number {
        return map_chartkit_format(format_settings);
    }
    field.map(field_format_options).unwrap_or(Value::Null)
}

fn default_date_format(data_type: &str) -> &'static str {
    if data_type == "date" {
        "DD.MM.YYYY"
    } else if data_type == "datetimetz" {
        "DD.MM.YYYY HH:mm:ss Z"
    } else {
        "DD.MM.YYYY HH:mm:ss"
    }
}

/// `getExportColumnSettings` (`utils/export-helpers.js`).
fn export_column_settings(path: &str, field: &Value) -> Value {
    let mut column_type = "text".to_string();
    let mut formatter = json!({});
    let mut format: Option<Value> = None;
    if is_number_field(field) {
        column_type = "number".to_string();
        formatter = get_format_options(field);
    } else if is_date_field(field) {
        column_type = s(field, "data_type").to_string();
        let field_format = s(field, "format");
        format = Some(if field_format.is_empty() {
            json!(default_date_format(s(field, "data_type")))
        } else {
            json!(field_format)
        });
    }
    let mut result = Map::new();
    result.insert("name".to_string(), json!(fake_title(field)));
    result.insert("formatter".to_string(), formatter);
    if let Some(format) = format {
        result.insert("format".to_string(), format);
    }
    result.insert("field".to_string(), json!(path));
    result.insert("type".to_string(), json!(column_type));
    Value::Object(result)
}

// ---------------------------------------------------------------------------------------------
// gravity base chart config (`gravity-charts/utils/index.js`)
// ---------------------------------------------------------------------------------------------

fn get_axis_title(settings: &Value, field: Option<&Value>) -> Option<Value> {
    match s(settings, "title") {
        "auto" => field.map(|field| json!(original_or_title(field))),
        "manual" => Some(json!(s(settings, "titleValue"))),
        _ => None,
    }
}

fn is_grid_enabled(settings: &Value) -> bool {
    s(settings, "grid") != "off"
}

fn tick_pixel_interval(settings: &Value) -> Option<Value> {
    if s(settings, "grid") == "on" && s(settings, "gridStep") == "manual" {
        settings.get("gridStepValue").cloned()
    } else {
        None
    }
}

fn labels_rotation(settings: &Value) -> Option<Value> {
    match s(settings, "labelsView") {
        "horizontal" => Some(json!(0)),
        "vertical" => Some(json!(90)),
        "angle" => Some(json!(45)),
        _ => None,
    }
}

fn axis_min_max(settings: &Value) -> (Option<Value>, Option<Value>) {
    if s(settings, "scale") == "manual" {
        if let Some(values) = settings.get("scaleValue").and_then(Value::as_array) {
            let min = values.first().and_then(number_value);
            let max = values.get(1).and_then(number_value);
            return (min.map(|value| json!(value)), max.map(|value| json!(value)));
        }
        return (None, None);
    }
    if s(settings, "scaleValue") == "0-max" {
        return (Some(json!(0)), None);
    }
    (None, None)
}

fn axis_settings(placeholder: Option<&Value>) -> Value {
    placeholder
        .and_then(|placeholder| placeholder.get("settings"))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn axis_base(placeholder: Option<&Value>, tick_default: Option<i64>) -> Map<String, Value> {
    let settings = axis_settings(placeholder);
    let item = placeholder.and_then(|placeholder| array(placeholder, "items").first());
    let (min, max) = axis_min_max(&settings);
    let mut axis = Map::new();
    axis.insert(
        "type".to_string(),
        json!(if item.map(is_date_field).unwrap_or(false) {
            "datetime"
        } else {
            "linear"
        }),
    );
    axis.insert("visible".to_string(), json!(s(&settings, "axisVisibility") != "hide"));
    let mut labels = Map::new();
    labels.insert(
        "enabled".to_string(),
        json!(item.is_some() && s(&settings, "hideLabels") != "yes"),
    );
    if let Some(rotation) = labels_rotation(&settings) {
        labels.insert("rotation".to_string(), rotation);
    }
    axis.insert("labels".to_string(), Value::Object(labels));
    axis.insert(
        "title".to_string(),
        match get_axis_title(&settings, item) {
            Some(text) => json!({"text": text}),
            None => json!({}),
        },
    );
    axis.insert(
        "grid".to_string(),
        json!({"enabled": item.is_some() && is_grid_enabled(&settings)}),
    );
    let interval = tick_pixel_interval(&settings).or_else(|| tick_default.map(|value| json!(value)));
    axis.insert(
        "ticks".to_string(),
        match interval {
            Some(interval) => json!({"pixelInterval": interval}),
            None => json!({}),
        },
    );
    axis.insert(
        "lineColor".to_string(),
        json!("var(--g-color-line-generic)"),
    );
    if let Some(min) = min {
        axis.insert("min".to_string(), min);
    }
    if let Some(max) = max {
        axis.insert("max".to_string(), max);
    }
    axis
}

/// `getBaseChartConfig` (`gravity-charts/utils/index.js`) restricted to the plain `bar`
/// visualization.
fn get_base_chart_config(config: &Value) -> Result<Value, String> {
    let visualization = config
        .get("visualization")
        .ok_or_else(|| "gravity bar: visualization is missing".to_string())?;
    let extra = config
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if s(&extra, "navigatorMode") == "show" {
        return Err("gravity bar: navigator is not ported".to_string());
    }
    let placeholders = array(visualization, "placeholders");

    let mut tooltip = json!({"enabled": s(&extra, "tooltip") != "hide"});
    tooltip["totals"] = json!({"enabled": s(&extra, "tooltipSum") != "off"});
    let mut legend = json!({});
    if s(&extra, "legendMode") == "hide" {
        legend["enabled"] = json!(false);
    }

    let mut base = Map::new();
    let title = s(&extra, "title");
    if s(&extra, "titleMode") != "hide" && !title.is_empty() {
        base.insert("title".to_string(), json!({"text": title}));
    }
    base.insert("tooltip".to_string(), tooltip);
    base.insert("legend".to_string(), legend);
    base.insert(
        "series".to_string(),
        json!({
            "data": [],
            "options": {
                "bar-x": {
                    "barMaxWidth": 50,
                    "barPadding": 0.05,
                    "groupPadding": 0.4,
                    "dataSorting": {"direction": "desc", "key": "name"}
                },
                "line": {"lineWidth": 2},
                "area": {"lineWidth": 2}
            }
        }),
    );
    base.insert(
        "chart".to_string(),
        json!({
            "margin": {"top": 10, "left": 10, "right": 10, "bottom": 15},
            "zoom": {
                "enabled": true,
                "resetButton": {
                    "align": "top-right",
                    "offset": {"x": 2, "y": 30},
                    "relativeTo": "plot-box"
                }
            }
        }),
    );

    // `bar`/`bar100p` keep the value axis on x and the category axis on y.
    let mut x_axis = axis_base(find_placeholder(placeholders, "x"), None);
    x_axis.insert("lineColor".to_string(), json!("transparent"));
    base.insert("xAxis".to_string(), Value::Object(x_axis));
    base.insert(
        "yAxis".to_string(),
        Value::Array(vec![Value::Object(axis_base(
            find_placeholder(placeholders, "y"),
            Some(72),
        ))]),
    );
    Ok(Value::Object(base))
}

// ---------------------------------------------------------------------------------------------
// palettes (`server/constants/index.js:selectServerPalette`, `utils/color-helpers.js`)
// ---------------------------------------------------------------------------------------------

fn select_server_palette(palette: &str, default_palette_id: &str) -> Vec<String> {
    let palettes = graph_engine::system_palettes();
    let pick = |id: &str| -> Option<Vec<String>> {
        if id.is_empty() {
            return None;
        }
        palettes
            .get(id)
            .and_then(|entry| entry.get("scheme"))
            .and_then(Value::as_array)
            .map(|scheme| scheme.iter().filter_map(Value::as_str).map(str::to_string).collect())
    };
    pick(palette)
        .or_else(|| pick(default_palette_id))
        .unwrap_or_default()
}

fn colors_settings(
    colors_config: &Value,
    color_field: Option<&Value>,
    default_palette_id: &str,
) -> (Map<String, Value>, Vec<String>) {
    let mut mounted = Map::new();
    let mut colors: Vec<String> = Vec::new();
    let field_guid = color_field.map(|field| s(field, "guid")).unwrap_or("");
    let colored_by_measure = colors_config
        .get("coloredByMeasure")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(mounted_colors) = colors_config.get("mountedColors").and_then(Value::as_object) {
        if field_guid == s(colors_config, "fieldGuid") || colored_by_measure {
            mounted = mounted_colors.clone();
        }
    }
    if colors.is_empty() {
        colors = select_server_palette(s(colors_config, "palette"), default_palette_id);
    }
    (mounted, colors)
}

fn mounted_color(mounted: &Map<String, Value>, colors: &[String], value: &str) -> Option<String> {
    let color = mounted.get(value)?.clone();
    match color.as_str() {
        Some(text) => match text.parse::<usize>() {
            Ok(index) if !colors.is_empty() => Some(colors[index % colors.len()].clone()),
            _ => Some(text.to_string()),
        },
        None => color
            .as_u64()
            .and_then(|index| colors.get(index as usize % colors.len()).cloned()),
    }
}

fn get_color(index: usize, colors: &[String]) -> String {
    if colors.is_empty() {
        return String::new();
    }
    colors[index % colors.len()].clone()
}

/// `mapAndColorizeGraphsByPalette` for the no-colour-field, single-series case.
fn colorize_graphs(graphs: &mut [Value], colors_config: &Value, default_palette_id: &str) {
    let (mounted, colors) = colors_settings(colors_config, None, default_palette_id);
    let mut used_colors: Vec<String> = Vec::new();
    for (index, graph) in graphs.iter_mut().enumerate() {
        let color_title = graph
            .get("colorKey")
            .and_then(Value::as_str)
            .or_else(|| graph.get("legendTitle").and_then(Value::as_str))
            .or_else(|| graph.get("name").and_then(Value::as_str))
            .unwrap_or("");
        if !color_title.is_empty() {
            if let Some(color) = mounted_color(&mounted, &colors, color_title) {
                graph["color"] = json!(color);
                continue;
            }
        }
        let value = graph
            .get("colorValue")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut color_index = used_colors.iter().position(|used| used == &value);
        if color_index.is_none() {
            used_colors.push(value);
            color_index = Some(used_colors.len() - 1);
        }
        let color_index = color_index.unwrap_or(index);
        graph["color"] = json!(get_color(color_index, &colors));
    }
}

// ---------------------------------------------------------------------------------------------
// lodash `merge`
// ---------------------------------------------------------------------------------------------

fn merge_json(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(target), Value::Array(source)) => {
            for (index, value) in source.into_iter().enumerate() {
                if index < target.len() {
                    merge_json(&mut target[index], value);
                } else {
                    target.push(value);
                }
            }
        }
        (target, source) => *target = source,
    }
}

// ---------------------------------------------------------------------------------------------
// data-api response helpers
// ---------------------------------------------------------------------------------------------

struct ResultTable {
    rows: Vec<Value>,
    id_to_title: HashMap<String, String>,
    id_to_data_type: HashMap<String, String>,
    order: Vec<(String, String)>, // (title, dataset id)
    dataset_id: String,
}

fn result_table(response: &Value, dataset: &str) -> Result<ResultTable, String> {
    let fields = array(response, "fields");
    let mut id_to_title = HashMap::new();
    let mut id_to_data_type = HashMap::new();
    let mut by_legend: HashMap<i64, (String, String)> = HashMap::new();
    for field in fields {
        let guid = s(field, "id").to_string();
        id_to_title.insert(guid.clone(), s(field, "title").to_string());
        id_to_data_type.insert(guid.clone(), s(field, "data_type").to_string());
        if let Some(legend) = field.get("legend_item_id").and_then(Value::as_i64) {
            by_legend
                .entry(legend)
                .or_insert_with(|| {
                    (s(field, "title").to_string(), s(field, "data_type").to_string())
                });
        }
    }
    let rows = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "gravity bar: data-api rows are missing".to_string())?;
    let mut order = Vec::new();
    if let Some(legend) = rows
        .first()
        .and_then(|row| row.get("legend"))
        .and_then(Value::as_array)
    {
        for item in legend {
            let Some(legend_id) = item.as_i64() else { continue };
            if let Some((title, _data_type)) = by_legend.get(&legend_id) {
                order.push((title.clone(), dataset.to_string()));
            }
        }
    }
    Ok(ResultTable {
        rows,
        id_to_title,
        id_to_data_type,
        order,
        dataset_id: dataset.to_string(),
    })
}

impl ResultTable {
    fn index_of(&self, field: &Value) -> Option<usize> {
        let guid = s(field, "guid");
        let title = self
            .id_to_title
            .get(guid)
            .cloned()
            .unwrap_or_else(|| s(field, "title").to_string());
        let dataset = if s(field, "datasetId").is_empty() {
            self.dataset_id.clone()
        } else {
            s(field, "datasetId").to_string()
        };
        self.order
            .iter()
            .position(|(order_title, order_dataset)| order_title == &title && order_dataset == &dataset)
    }
}

// ---------------------------------------------------------------------------------------------
// public interface
// ---------------------------------------------------------------------------------------------

pub struct BarFinished {
    pub data: Value,
    pub config: Value,
    pub library: Value,
    pub sources: Value,
}

/// Build the migrated config and the data-api request for the Gravity `bar` chart.
///
/// The returned shape is the same [`graph_engine::GraphPrepared`] the chartkit renderer uses, so
/// `run_rust_graph` can share its dataset fetch / data-api fetch loop for both renderers.
pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &HashMap<String, DatasetInfo>,
) -> Result<graph_engine::GraphPrepared, String> {
    let mut prepared = graph_engine::prepare(shared, params, datasets)?;
    // v9: the builder covers the common bar shapes; settings it does not render are
    // normalised away here instead of rejecting the whole chart (previously a hard guard).
    // Anything that still cannot be represented falls back to the classic Highcharts
    // conversion in `run_rust_graph`.
    sanitize_config(&mut prepared.config);
    Ok(prepared)
}

/// Best-effort normalisation of UI bar configs.
///
/// * layered configs: the first layer's placeholders are promoted (charts stay single-layer);
/// * placeholders are normalised to the `y` (categories) / `x` (measure) order the builder
///   expects; a second category item becomes the implicit stack/colour dimension;
/// * unsupported visual settings are dropped rather than rejected (labels tooltips sort
///   shapes segments axis formatting, colour-by-measure gradients keep their palette path).
fn sanitize_config(config: &mut Value) {
    let Some(visualization) = config.get_mut("visualization") else { return };
    if let Some(layers) = visualization.get("layers").and_then(Value::as_array).cloned() {
        if let Some(first) = layers.first() {
            if let Some(placeholders) = first.get("placeholders").cloned() {
                visualization["placeholders"] = placeholders;
            }
        }
        visualization["layers"] = json!([]);
    }
    let placeholders = array(visualization, "placeholders").to_vec();
    let mut y = find_placeholder(&placeholders, "y").cloned().unwrap_or_else(|| json!({"id": "y", "items": []}));
    let mut x = find_placeholder(&placeholders, "x").cloned().unwrap_or_else(|| json!({"id": "x", "items": []}));
    let y_items: Vec<Value> = array(&y, "items")
        .iter()
        .filter(|item| !is_pseudo_field(item))
        .take(2)
        .cloned()
        .collect();
    let x_items: Vec<Value> = array(&x, "items")
        .iter()
        .filter(|item| !is_pseudo_field(item))
        .find(|item| is_number_field(item))
        .cloned()
        .or_else(|| {
            array(&x, "items")
                .iter()
                .find(|item| !is_pseudo_field(item))
                .cloned()
        })
        .into_iter()
        .collect();
    y["items"] = Value::Array(y_items.clone());
    x["items"] = Value::Array(x_items.clone());
    visualization["placeholders"] = json!([y, x]);
    // The second category item is the stack dimension; the colour builder understands a
    // colour field, so hand it the item when the chart has no explicit colour field.
    if y_items.len() > 1 && array(config, "colors").is_empty() {
        config["colors"] = json!([y_items[1].clone()]);
    }
    // One string colour dimension; measure/gradient colours keep the palette path.
    let mut colors: Vec<Value> = array(config, "colors")
        .iter()
        .filter(|color| {
            !is_pseudo_field(color) && !is_number_field(color) && !is_date_field(color)
        })
        .take(1)
        .cloned()
        .collect();
    if colors.is_empty() {
        colors = Vec::new();
    }
    config["colors"] = Value::Array(colors);
    if let Some(settings) = config.get_mut("colorsConfig").and_then(Value::as_object_mut) {
        for key in ["colorMode", "gradientMode", "gradientPalette", "gradientColors"] {
            settings.remove(key);
        }
    }
    // The builder renders the first numeric label / first sort item; extra entries are
    // client-side settings and must not reject the chart.
    let labels: Vec<Value> = array(config, "labels")
        .iter()
        .filter(|label| is_number_field(label) || is_date_field(label))
        .take(1)
        .cloned()
        .collect();
    config["labels"] = Value::Array(labels);
    let sorts: Vec<Value> = array(config, "sort").iter().take(1).cloned().collect();
    config["sort"] = Value::Array(sorts);
    config["tooltips"] = json!([]);
    config["shapes"] = json!([]);
    config["segments"] = json!([]);
    if let Some(map) = config.as_object_mut() {
        map.remove("tooltipConfig");
    }
    if let Some(extra) = config.get_mut("extraSettings").and_then(Value::as_object_mut) {
        if extra.contains_key("labelsPosition") {
            extra.insert("labelsPosition".to_string(), json!(""));
        }
    }
    // Axis label formatting is rendered from `axisFormatMode` (`axis_base` keeps the rest);
    // the axis mode map only drives chartkit axis types, which the gravity builder derives
    // from the field types itself.
    if let Some(placeholders) = config
        .pointer_mut("/visualization/placeholders")
        .and_then(Value::as_array_mut)
    {
        for placeholder in placeholders.iter_mut() {
            if let Some(settings) = placeholder.get_mut("settings").and_then(Value::as_object_mut) {
                if settings.contains_key("type") {
                    settings.insert("type".to_string(), json!("linear"));
                }
                settings.remove("axisModeMap");
            }
        }
    }
}

/// `getAxisFormatting` (`preparers/helpers/axis/get-axis-formatting.ts`), shared by the
/// Gravity renderers: `numberFormat` for a measure axis, `None` for `auto` and for percent
/// visualizations.
pub(crate) fn axis_formatting(
    placeholder: Option<&Value>,
    visualization_id: &str,
) -> Option<Value> {
    let placeholder = placeholder?;
    let field = array(placeholder, "items").first()?;
    if !is_number_field(field) || matches!(visualization_id, "bar100p" | "column100p" | "area100p")
    {
        return None;
    }
    let settings = placeholder.get("settings").cloned().unwrap_or_else(|| json!({}));
    match s(&settings, "axisFormatMode") {
        "by-field" => Some(get_format_options(field)),
        "manual" => Some(
            settings
                .get("axisLabelFormating")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or_else(|| json!({})),
        ),
        _ => None,
    }
}


/// Build the Gravity `data` payload, the envelope `config` and the `sources` member.
pub fn finish(
    prepared: &graph_engine::GraphPrepared,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<BarFinished, String> {
    let data = build_data(&prepared.config, fetched, default_palette_id)?;
    let config = build_envelope_config(&prepared.config, &prepared.params);
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
    Ok(BarFinished {
        data,
        config,
        library: json!({}),
        sources: Value::Object(sources),
    })
}

// ---------------------------------------------------------------------------------------------
// `prepareBarYData` + `prepareGravityChartsBarY`
// ---------------------------------------------------------------------------------------------

fn build_data(
    config: &Value,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<Value, String> {
    let visualization_id = config
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let has_colors = !array(config, "colors").is_empty();
    if visualization_id == "bar100p" && has_colors {
        return build_color_bar_data(config, fetched, default_palette_id, true);
    }
    if visualization_id == "bar" && has_colors {
        return build_color_bar_data(config, fetched, default_palette_id, false);
    }
    let response = fetched
        .first()
        .ok_or_else(|| "gravity bar: no fetched source".to_string())?
        .body
        .clone();
    let visualization = config
        .get("visualization")
        .ok_or_else(|| "gravity bar: visualization is missing".to_string())?;
    let placeholders = array(visualization, "placeholders");
    let category_placeholder = &placeholders[0];
    let measure_placeholder = &placeholders[1];
    let category_field = array(category_placeholder, "items")
        .first()
        .cloned()
        .ok_or_else(|| "gravity bar: category field is missing".to_string())?;
    let measure_field = array(measure_placeholder, "items")
        .first()
        .cloned()
        .ok_or_else(|| "gravity bar: measure field is missing".to_string())?;
    let label_field = array(config, "labels").first().cloned();
    let sort_item = array(config, "sort").first().cloned();
    let extra = config
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let dataset = array(config, "datasetsIds")
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| "gravity bar: dataset is missing".to_string())?
        .to_string();
    let table = result_table(&response, &dataset)?;
    let category_index = table
        .index_of(&category_field)
        .ok_or_else(|| "gravity bar: category field is not in the response".to_string())?;
    let measure_index = table
        .index_of(&measure_field)
        .ok_or_else(|| "gravity bar: measure field is not in the response".to_string())?;
    let mut label_field = label_field;
    let label_index = match &label_field {
        Some(label) => {
            let index = table.index_of(label);
            if index.is_none() {
                // A stale label placeholder (field absent from the data-api response) disables
                // labels instead of falling back to the classic renderer.
                label_field = None;
            }
            index
        }
        None => None,
    };
    let measure_data_type = table
        .id_to_data_type
        .get(s(&measure_field, "guid"))
        .cloned()
        .unwrap_or_else(|| s(&measure_field, "data_type").to_string());
    let label_data_type = label_field
        .as_ref()
        .map(|label| {
            table
                .id_to_data_type
                .get(s(label, "guid"))
                .cloned()
                .unwrap_or_else(|| s(label, "data_type").to_string())
        })
        .unwrap_or_default();
    let nulls = s(
        &axis_settings(find_placeholder(placeholders, "x")),
        "nulls",
    )
    .to_string();

    // category -> (value, label) for the single line
    let mut categories: Vec<Value> = Vec::new();
    let mut categories_seen: Vec<String> = Vec::new();
    let mut line_data: HashMap<String, (Value, Option<Value>)> = HashMap::new();
    for row in &table.rows {
        let Some(values) = row.get("data").and_then(Value::as_array) else {
            continue;
        };
        let Some(category) = values.get(category_index) else { continue };
        if category.is_null() {
            continue;
        }
        let category_key = match category {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if !categories_seen.contains(&category_key) {
            categories_seen.push(category_key.clone());
            categories.push(category.clone());
        }
        let Some(raw) = values.get(measure_index) else { continue };
        let value = if raw.is_null() {
            if nulls == "as-0" {
                json!(0)
            } else {
                Value::Null
            }
        } else {
            let number = number_value(raw)
                .ok_or_else(|| "gravity bar: measure value is not numeric".to_string())?;
            json!(number)
        };
        let label = match label_index {
            Some(index) => match values.get(index) {
                None | Some(Value::Null) => None,
                Some(raw) => Some(json!(number_value(raw).ok_or_else(|| {
                    "gravity bar: label value is not numeric".to_string()
                })?)),
            },
            None => None,
        };
        line_data.insert(category_key, (value, label));
    }

    // `getSortedCategories` for a single line and a measure sort item.
    if let Some(sort_item) = &sort_item {
        let sort_title = original_or_title(sort_item);
        let sort_source = s(sort_item, "source");
        let matched = original_or_title(&measure_field) == sort_title
            || (!sort_source.is_empty() && sort_source == s(&measure_field, "source"));
        if matched {
            let is_asc = s(sort_item, "direction") == "ASC";
            categories.sort_by(|a, b| {
                let a_key = match a {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let b_key = match b {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let a_value = line_data.get(&a_key).and_then(|(value, _)| number_value(value));
                let b_value = line_data.get(&b_key).and_then(|(value, _)| number_value(value));
                let greater = match (a_value, b_value) {
                    (Some(a_value), Some(b_value)) => a_value > b_value,
                    _ => false,
                };
                if is_asc {
                    if greater {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    }
                } else if greater {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            });
        }
    }

    let displayed_title = table
        .id_to_title
        .get(s(&measure_field, "guid"))
        .cloned()
        .unwrap_or_else(|| fake_title(&measure_field));
    let measure_field_title = fake_title(&measure_field);
    let measure_formatting = get_format_options(&measure_field);
    let line_tooltip = chartkit_format(non_empty_object(&measure_formatting), &measure_data_type);
    let label_formatting = label_field
        .as_ref()
        .map(get_format_options)
        .unwrap_or(Value::Null);
    let line_data_labels = if label_field.is_some() {
        chartkit_format(non_empty_object(&label_formatting), &label_data_type)
    } else {
        json!({"enabled": false})
    };

    // `prepareBarYData` graph for the single line
    let mut points = Vec::new();
    for (index, category) in categories.iter().enumerate() {
        let category_key = match category {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let Some((value, label)) = line_data.get(&category_key) else { continue };
        let mut point = Map::new();
        point.insert("x".to_string(), json!(index));
        point.insert("y".to_string(), value.clone());
        point.insert(
            "label".to_string(),
            label.clone().unwrap_or_else(|| json!("")),
        );
        points.push(Value::Object(point));
    }
    let mut graph = Map::new();
    graph.insert("id".to_string(), json!(displayed_title));
    graph.insert("title".to_string(), json!(displayed_title));
    graph.insert("tooltip".to_string(), line_tooltip);
    let mut graph_data_labels = Map::new();
    graph_data_labels.insert("enabled".to_string(), json!(label_field.is_some()));
    if let Value::Object(labels) = &line_data_labels {
        for (key, value) in labels {
            if key.as_str() != "enabled" {
                graph_data_labels.insert(key.clone(), value.clone());
            }
        }
    }
    graph_data_labels.insert("useHTML".to_string(), json!(false));
    graph.insert("dataLabels".to_string(), Value::Object(graph_data_labels));
    graph.insert("data".to_string(), Value::Array(points));
    graph.insert("legendTitle".to_string(), json!(displayed_title));
    graph.insert("colorKey".to_string(), json!(measure_field_title));
    graph.insert("colorGuid".to_string(), Value::Null);
    graph.insert("shapeGuid".to_string(), Value::Null);
    graph.insert("connectNulls".to_string(), json!(nulls == "connect"));
    graph.insert("measureFieldTitle".to_string(), json!(measure_field_title));
    graph.insert("yAxis".to_string(), json!(0));
    graph.insert("custom".to_string(), json!({}));
    let mut graphs = vec![Value::Object(graph)];
    let colors_config = config.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    colorize_graphs(&mut graphs, &colors_config, default_palette_id);
    let graph = &graphs[0];
    let graph_data_labels = graph.get("dataLabels").cloned().unwrap_or_else(|| json!({}));
    let graph_tooltip = graph.get("tooltip").cloned().unwrap_or_else(|| json!({}));

    // `prepareGravityChartsBarY` series transform
    let mut series_data: Vec<Value> = Vec::new();
    for point in array(graph, "data") {
        let py = point.get("y").cloned().unwrap_or(Value::Null);
        if py.is_null() {
            // Render a gap rather than rejecting the chart (`nulls` modes are formatting).
            continue;
        }
        let Some(py_number) = number_value(&py) else {
            continue;
        };
        let Some(px_index) = point.get("x").and_then(Value::as_i64) else {
            return Err("gravity bar: point index is not an integer".to_string());
        };
        // Single graph in scope: `total` is this point's value and `percentage` is 100.
        let total = py_number;
        let percentage = py_number / total * 100.0;
        let mut output = Map::new();
        output.insert("y".to_string(), json!(px_index));
        output.insert("x".to_string(), json!(py_number));
        output.insert(
            "label".to_string(),
            point.get("label").cloned().unwrap_or_else(|| json!("")),
        );
        output.insert("total".to_string(), json!(total));
        // `JSON.stringify(100)` renders `100`, not `100.0`: integral doubles stay integers.
        output.insert(
            "percentage".to_string(),
            crate::gravity_donut::js_number_from_f64(percentage),
        );
        if let Some(color) = point.get("color") {
            output.insert("color".to_string(), color.clone());
        }
        series_data.push(Value::Object(output));
    }
    let should_use_percent_stacking = visualization_id == "bar100p";
    let data_labels_inside = s(&extra, "labelsPosition") != "outside";
    let should_use_html_for_labels = false;
    let label_value_format = if label_field.is_some() {
        map_to_gravity_value_format(label_field.as_ref(), non_empty_object(&graph_data_labels))
    } else {
        Value::Null
    };

    let export_settings = json!({
        "columns": [
            export_column_settings("category", &category_field),
            export_column_settings("x", &measure_field),
        ]
    });

    let mut series_config = Map::new();
    if let Value::Object(spread) = graph {
        for (key, value) in spread {
            series_config.insert(key.clone(), value.clone());
        }
    }
    series_config.insert("type".to_string(), json!("bar-y"));
    series_config.insert(
        "stacking".to_string(),
        json!(if should_use_percent_stacking { "percent" } else { "normal" }),
    );
    series_config.insert("name".to_string(), json!(displayed_title));
    series_config.insert("data".to_string(), Value::Array(series_data));
    let mut series_data_labels = Map::new();
    series_data_labels.insert("enabled".to_string(), json!(label_field.is_some()));
    series_data_labels.insert("inside".to_string(), json!(data_labels_inside));
    series_data_labels.insert("html".to_string(), json!(should_use_html_for_labels));
    if !label_value_format.is_null() {
        series_data_labels.insert("format".to_string(), label_value_format);
    }
    series_config.insert("dataLabels".to_string(), Value::Object(series_data_labels));
    if graph_tooltip.get("chartKitFormatting").and_then(Value::as_bool).unwrap_or(false) {
        series_config.insert(
            "tooltip".to_string(),
            json!({"valueFormat": map_chartkit_format(&graph_tooltip)}),
        );
    }
    let mut series_custom = graph
        .get("custom")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(color_value) = graph.get("colorValue") {
        series_custom.insert("colorValue".to_string(), color_value.clone());
    }
    series_custom.insert("exportSettings".to_string(), export_settings);
    series_config.insert("custom".to_string(), Value::Object(series_custom));

    let categories_formatted: Vec<Value> = categories
        .iter()
        .map(|category| {
            let text = match category {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            json!(if text.is_empty() { "Null".to_string() } else { text })
        })
        .collect();
    let category_settings = axis_settings(find_placeholder(placeholders, "y"));
    let mut gravity_config = Map::new();
    gravity_config.insert(
        "series".to_string(),
        json!({
            "data": [Value::Object(series_config)],
            "options": {"bar-y": {"stackGap": 0, "borderWidth": 1}}
        }),
    );
    let mut x_axis = Map::new();
    x_axis.insert("type".to_string(), json!("linear"));
    if let Some(formatting) = axis_formatting(find_placeholder(placeholders, "x"), visualization_id)
    {
        x_axis.insert("labels".to_string(), json!({"numberFormat": formatting}));
    }
    gravity_config.insert("xAxis".to_string(), Value::Object(x_axis));
    let mut legend = Map::new();
    legend.insert("html".to_string(), json!(false));
    if graphs.len() <= 1 {
        legend.insert("enabled".to_string(), json!(false));
    }
    gravity_config.insert("legend".to_string(), Value::Object(legend));
    let value_format = field_format_options(&measure_field);
    if !value_format.is_null() {
        gravity_config.insert(
            "tooltip".to_string(),
            json!({"valueFormat": value_format}),
        );
    }
    gravity_config.insert(
        "custom".to_string(),
        json!({"tooltip": {"headerLabel": fake_title(&category_field)}}),
    );
    gravity_config.insert(
        "yAxis".to_string(),
        json!([{
            "type": "category",
            "categories": categories_formatted,
            "order": "reverse",
            "labels": {
                "enabled": s(&category_settings, "hideLabels") != "yes",
                "html": false,
                "maxWidth": "33%",
                "padding": 0
            }
        }]),
    );

    let mut data = get_base_chart_config(config)?;
    merge_json(&mut data, Value::Object(gravity_config));
    Ok(data)
}

// ---------------------------------------------------------------------------------------------
// `bar`/`bar100p` colour split (`prepareBarYData` + `prepareGravityChartsBarY`)
// ---------------------------------------------------------------------------------------------

/// Gravity bar-y with a colour dimension: one series per colour value. `bar100p` is
/// percent-stacked with percentage labels; plain `bar` keeps raw labels and (with a second
/// category placeholder) a stacked series.
fn build_color_bar_data(
    config: &Value,
    fetched: &[GraphFetched],
    default_palette_id: &str,
    percent_stacking: bool,
) -> Result<Value, String> {
    let response = fetched
        .first()
        .ok_or_else(|| "gravity bar: no fetched source".to_string())?
        .body
        .clone();
    let visualization = config
        .get("visualization")
        .ok_or_else(|| "gravity bar: visualization is missing".to_string())?;
    let placeholders = array(visualization, "placeholders");
    let visualization_id = s(visualization, "id").to_string();
    let category_placeholder = find_placeholder(placeholders, "y")
        .ok_or_else(|| "gravity bar: y placeholder is missing".to_string())?;
    let measure_placeholder = find_placeholder(placeholders, "x")
        .ok_or_else(|| "gravity bar: x placeholder is missing".to_string())?;
    let category_items = array(category_placeholder, "items");
    let category_field = category_items
        .first()
        .cloned()
        .ok_or_else(|| "gravity bar: category field is missing".to_string())?;
    let measure_field = array(measure_placeholder, "items")
        .first()
        .cloned()
        .ok_or_else(|| "gravity bar: measure field is missing".to_string())?;
    let color_field = array(config, "colors")
        .first()
        .cloned()
        .ok_or_else(|| "gravity bar: colour field is missing".to_string())?;
    let has_stack = category_items.len() > 1;
    let label_field = array(config, "labels").first().cloned();
    let sort_item = array(config, "sort").first().cloned();
    let extra = config
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dataset = array(config, "datasetsIds")
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| "gravity bar: dataset is missing".to_string())?
        .to_string();
    let table = result_table(&response, &dataset)?;
    let category_index = table
        .index_of(&category_field)
        .ok_or_else(|| "gravity bar: category field is not in the response".to_string())?;
    let measure_index = table
        .index_of(&measure_field)
        .ok_or_else(|| "gravity bar: measure field is not in the response".to_string())?;
    let color_index = table
        .index_of(&color_field)
        .ok_or_else(|| "gravity bar: colour field is not in the response".to_string())?;
    let mut label_field = label_field;
    let label_index = match &label_field {
        Some(label) => {
            let index = table.index_of(label);
            if index.is_none() {
                // A stale label placeholder (field absent from the data-api response) disables
                // labels instead of falling back to the classic renderer.
                label_field = None;
            }
            index
        }
        None => None,
    };
    let measure_data_type = table
        .id_to_data_type
        .get(s(&measure_field, "guid"))
        .cloned()
        .unwrap_or_else(|| s(&measure_field, "data_type").to_string());
    let label_data_type = label_field
        .as_ref()
        .map(|label| {
            table
                .id_to_data_type
                .get(s(label, "guid"))
                .cloned()
                .unwrap_or_else(|| s(label, "data_type").to_string())
        })
        .unwrap_or_default();
    let nulls = s(
        &axis_settings(find_placeholder(placeholders, "x")),
        "nulls",
    )
    .to_string();

    // First-appearance ordering before the JS collator sorts.
    let mut categories: Vec<String> = Vec::new();
    let mut category_indexes: HashMap<String, usize> = HashMap::new();
    let mut color_order: Vec<String> = Vec::new();
    // (category -> (value, label)) per colour value, plus the raw row order.
    let mut color_rows: HashMap<String, Vec<(usize, Option<f64>, Option<f64>)>> = HashMap::new();
    for row in &table.rows {
        let Some(values) = row.get("data").and_then(Value::as_array) else {
            continue;
        };
        let Some(category) = values.get(category_index) else { continue };
        if category.is_null() {
            continue;
        }
        let category_key = match category {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let position = match category_indexes.get(&category_key) {
            Some(position) => *position,
            None => {
                let position = categories.len();
                category_indexes.insert(category_key.clone(), position);
                categories.push(category_key.clone());
                position
            }
        };
        let Some(raw_measure) = values.get(measure_index) else { continue };
        let _ = &nulls;
        let value = if raw_measure.is_null() {
            None
        } else {
            Some(
                number_value(raw_measure)
                    .ok_or_else(|| "gravity bar: measure value is not numeric".to_string())?,
            )
        };
        let Some(raw_color) = values.get(color_index) else { continue };
        if raw_color.is_null() {
            continue;
        }
        let color_value = match raw_color {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let label = label_index.and_then(|index| values.get(index).and_then(number_value));
        if !color_rows.contains_key(&color_value) {
            color_order.push(color_value.clone());
            color_rows.insert(color_value.clone(), Vec::new());
        }
        if let Some(rows) = color_rows.get_mut(&color_value) {
            rows.push((position, value, label));
        }
    }
    if categories.is_empty() || color_order.is_empty() {
        return Err("gravity bar: no series data".to_string());
    }

    let sort_guid = sort_item
        .as_ref()
        .map(|sort| s(sort, "guid").to_string())
        .unwrap_or_default();
    let measure_guid = s(&measure_field, "guid").to_string();
    let color_guid = s(&color_field, "guid").to_string();
    let is_measure_sort = !sort_guid.is_empty() && sort_guid == measure_guid;
    let is_color_sort = !sort_guid.is_empty() && sort_guid == color_guid;
    // `getSortedLineKeys`: no sort -> collator order; sort by colour -> collator + direction.
    let mut series_order = color_order.clone();
    if sort_item.is_none() {
        series_order.sort_by(|a, b| collator_compare(a, b));
    } else if is_color_sort {
        series_order.sort_by(|a, b| collator_compare(a, b));
        let descending = sort_item
            .as_ref()
            .map(|sort| s(sort, "direction") != "ASC")
            .unwrap_or(true);
        if descending {
            series_order.reverse();
        }
    }
    // `getSortedCategories`: no sort -> collator order; measure sort on a non-percent bar ->
    // sort by the category total (direction from the sort item).
    let mut category_order: Vec<usize> = (0..categories.len()).collect();
    if sort_item.is_none() {
        category_order.sort_by(|a, b| collator_compare(&categories[*a], &categories[*b]));
    } else if is_measure_sort && !percent_stacking {
        let mut totals = vec![0.0f64; categories.len()];
        for color_value in &series_order {
            if let Some(rows) = color_rows.get(color_value) {
                for (position, value, _) in rows {
                    totals[*position] += value.unwrap_or(0.0);
                }
            }
        }
        let descending = sort_item
            .as_ref()
            .map(|sort| s(sort, "direction") != "ASC")
            .unwrap_or(true);
        category_order.sort_by(|a, b| {
            let ordering = totals[*a]
                .partial_cmp(&totals[*b])
                .unwrap_or(std::cmp::Ordering::Equal);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    let category_rank: HashMap<usize, usize> = category_order
        .iter()
        .enumerate()
        .map(|(rank, position)| (*position, rank))
        .collect();
    let ordered_categories: Vec<String> = category_order
        .iter()
        .map(|position| categories[*position].clone())
        .collect();

    // Category totals are summed over the series in series order (`prepareGravityChartsBarY`).
    let mut category_totals = vec![0.0f64; categories.len()];
    for color_value in &series_order {
        if let Some(rows) = color_rows.get(color_value) {
            for (position, value, _) in rows {
                category_totals[*position] += value.unwrap_or(0.0);
            }
        }
    }

    let measure_formatting = get_format_options(&measure_field);
    let line_tooltip = chartkit_format(non_empty_object(&measure_formatting), &measure_data_type);
    let label_formatting = label_field
        .as_ref()
        .map(get_format_options)
        .unwrap_or(Value::Null);
    let label_mode_percent = s(&label_formatting, "labelMode") == "percent";
    let label_chartkit = label_field
        .as_ref()
        .map(|_| chartkit_format(non_empty_object(&label_formatting), &label_data_type))
        .unwrap_or(Value::Null);
    let label_value_format = match &label_field {
        Some(label) => map_to_gravity_value_format(Some(label), non_empty_object(&label_chartkit)),
        None => Value::Null,
    };
    let measure_field_title = fake_title(&measure_field);
    let export_settings = json!({
        "columns": [
            export_column_settings("category", &category_field),
            export_column_settings("x", &measure_field),
            export_column_settings("series.custom.colorValue", &color_field),
        ]
    });
    let colors_config = config.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    let (mounted, palette) = colors_settings(&colors_config, Some(&color_field), default_palette_id);
    let mut series_list: Vec<Value> = Vec::new();
    for (color_position, color_value) in series_order.iter().enumerate() {
        let Some(rows) = color_rows.get(color_value) else { continue };
        let lookup: HashMap<usize, (Option<f64>, Option<f64>)> = rows
            .iter()
            .map(|(position, value, label)| (*position, (*value, *label)))
            .collect();
        let color = mounted_color(&mounted, &palette, color_value)
            .unwrap_or_else(|| get_color(color_position, &palette));
        let series_data: Vec<Value> = category_order
            .iter()
            .map(|position| {
                let rank = category_rank[position];
                let (value, label) = lookup
                    .get(position)
                    .copied()
                    .unwrap_or((None, None));
                let total = category_totals[*position];
                let value_number = value.unwrap_or(0.0);
                let percentage = if total == 0.0 { 0.0 } else { value_number / total * 100.0 };
                let label_value = if percent_stacking && label_mode_percent {
                    json!(percentage)
                } else {
                    label.map(|label| json!(label)).unwrap_or_else(|| json!(""))
                };
                json!({
                    "y": rank,
                    "x": match value {
                        Some(value) => js_number_or_float(value),
                        None => Value::Null,
                    },
                    "label": label_value,
                    "total": total,
                    "percentage": percentage
                })
            })
            .collect();
        let mut data_labels = Map::new();
        data_labels.insert("enabled".to_string(), json!(label_field.is_some()));
        data_labels.insert(
            "inside".to_string(),
            json!(percent_stacking || s(&extra, "labelsPosition") != "outside"),
        );
        data_labels.insert("html".to_string(), json!(false));
        if !label_value_format.is_null() {
            data_labels.insert("format".to_string(), label_value_format.clone());
        }
        let mut series_custom = Map::new();
        series_custom.insert("colorValue".to_string(), json!(color_value));
        series_custom.insert("exportSettings".to_string(), export_settings.clone());
        let mut series = Map::new();
        series.insert("id".to_string(), json!(color_value));
        series.insert("title".to_string(), json!(color_value));
        if line_tooltip.get("chartKitFormatting").and_then(Value::as_bool).unwrap_or(false) {
            series.insert(
                "tooltip".to_string(),
                json!({"valueFormat": map_chartkit_format(&line_tooltip)}),
            );
        }
        series.insert("dataLabels".to_string(), Value::Object(data_labels));
        series.insert("data".to_string(), Value::Array(series_data));
        series.insert("legendTitle".to_string(), json!(color_value));
        if !has_stack {
            series.insert("drillDownFilterValue".to_string(), json!(color_value));
        }
        series.insert("colorGuid".to_string(), json!(color_guid));
        series.insert("shapeGuid".to_string(), Value::Null);
        series.insert("connectNulls".to_string(), json!(nulls == "connect"));
        series.insert("measureFieldTitle".to_string(), json!(measure_field_title));
        series.insert("yAxis".to_string(), json!(0));
        if has_stack {
            series.insert("stack".to_string(), json!(color_value));
        }
        series.insert("colorValue".to_string(), json!(color_value));
        series.insert("shapeValue".to_string(), json!(measure_field_title));
        series.insert("custom".to_string(), Value::Object(series_custom));
        series.insert("color".to_string(), json!(color));
        series.insert("type".to_string(), json!("bar-y"));
        if has_stack {
            series.insert("stackId".to_string(), json!(color_value));
        }
        series.insert(
            "stacking".to_string(),
            json!(if percent_stacking { "percent" } else { "normal" }),
        );
        series.insert("name".to_string(), json!(color_value));
        series_list.push(Value::Object(series));
    }

    let category_settings = axis_settings(find_placeholder(placeholders, "y"));
    let categories_formatted: Vec<Value> =
        ordered_categories.iter().map(|category| json!(category)).collect();
    let mut gravity_config = Map::new();
    gravity_config.insert(
        "series".to_string(),
        json!({
            "data": series_list,
            "options": {"bar-y": {"stackGap": 0, "borderWidth": 1}}
        }),
    );
    let mut x_axis = Map::new();
    x_axis.insert("type".to_string(), json!("linear"));
    if let Some(formatting) = axis_formatting(find_placeholder(placeholders, "x"), &visualization_id)
    {
        x_axis.insert("labels".to_string(), json!({"numberFormat": formatting}));
    }
    gravity_config.insert("xAxis".to_string(), Value::Object(x_axis));
    gravity_config.insert("legend".to_string(), json!({"html": false}));
    let value_format = field_format_options(&measure_field);
    if !value_format.is_null() {
        gravity_config.insert("tooltip".to_string(), json!({"valueFormat": value_format}));
    }
    gravity_config.insert(
        "custom".to_string(),
        json!({"tooltip": {"headerLabel": fake_title(&category_field)}}),
    );
    gravity_config.insert(
        "yAxis".to_string(),
        json!([{
            "type": "category",
            "categories": categories_formatted,
            "order": "reverse",
            "labels": {
                "enabled": s(&category_settings, "hideLabels") != "yes",
                "html": false,
                "maxWidth": "33%",
                "padding": 0
            }
        }]),
    );
    let mut data = get_base_chart_config(config)?;
    merge_json(&mut data, Value::Object(gravity_config));
    Ok(data)
}

fn collator_compare(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

fn js_number_or_float(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        return json!(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------------------------
// envelope `config` (`config/index.js:buildChartsConfigPrivate`)
// ---------------------------------------------------------------------------------------------

fn build_envelope_config(config: &Value, params: &Value) -> Value {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let id = s(&visualization, "id").to_string();
    let extra_value = config.get("extraSettings").cloned();
    let extra = extra_value.clone().unwrap_or_else(|| json!({}));
    let mut result = json!({
        "hideHolidaysBands": true,
        "linesLimit": 100,
        "tooltip": {"pin": {"altKey": true}, "sort": {"enabled": true}},
        "preventDefaultForPointClick": false,
    });
    if extra_value.map(|value| truthy(&value)).unwrap_or(false) {
        let title = s(&extra, "title");
        if !title.is_empty() && s(&extra, "titleMode") == "show" {
            result["title"] = json!(title);
        }
        let mut matched = Vec::new();
        if let Some(fields_by_dataset) = config.get("datasetsPartialFields").and_then(Value::as_array) {
            for fields in fields_by_dataset.iter().filter_map(Value::as_array) {
                for field in fields {
                    let title = s(field, "title");
                    if !title.is_empty() && params.get(title).map(truthy).unwrap_or(false) {
                        matched.push(json!(title));
                    }
                }
            }
        }
        result["comments"] = json!({"matchType": "intersection", "matchedParams": matched});
        let feed = s(&extra, "feed");
        if !feed.is_empty() {
            result["comments"]["feeds"] = json!([{
                "feed": feed,
                "matchedParams": matched,
                "matchType": "intersection"
            }]);
        }
    }
    if matches!(
        id.as_str(),
        "line" | "area" | "area100p" | "column" | "column100p" | "bar" | "bar100p"
    ) {
        result["manageTooltipConfig"] = json!("wizard-manage-tooltip-config");
        let tooltip_sum = extra.get("tooltipSum").and_then(Value::as_str);
        if tooltip_sum.is_none() || tooltip_sum == Some("on") {
            result["enableSum"] = json!(true);
        }
    } else if matches!(id.as_str(), "pie" | "donut") {
        result["showPercentInTooltip"] = json!(true);
        result["manageTooltipConfig"] = json!("wizard-manage-tooltip-config");
    }
    result
}


#[cfg(test)]
mod v9_tests {
    use super::*;
    use crate::graph_engine::GraphFetched;

    fn field(guid: &str, title: &str, data_type: &str, kind: &str, legend: i64) -> Value {
        json!({
            "legend_item_id": legend,
            "id": guid,
            "title": title,
            "data_type": data_type,
            "field_type": kind,
            "item_type": "field",
            "role_spec": {"role": if kind == "MEASURE" { "measure" } else { "row" }},
        })
    }

    fn fetched() -> Vec<GraphFetched> {
        let body = json!({
            "fields": [
                field("f_region", "Region", "string", "DIMENSION", 0),
                field("f_year", "Year", "string", "DIMENSION", 1),
                field("f_sales", "Sales", "integer", "MEASURE", 2),
            ],
            "result_data": [{"rows": [
                {"data": ["East", "2019", "100"], "legend": [0, 1, 2]},
                {"data": ["North", "2019", "200"], "legend": [0, 1, 2]},
                {"data": ["East", "2020", "300"], "legend": [0, 1, 2]},
            ]}],
            "blocks": [],
            "notifications": [],
            "data_export": {"background": {"allowed": true, "reason": null}, "basic": {"allowed": true, "reason": null}},
            "data_export_forbidden": false,
        });
        vec![GraphFetched {
            key: "demo_result".to_string(),
            dataset: "demo".to_string(),
            status: 200,
            latency: 0.0,
            size: 0,
            body,
            data_export: None,
        }]
    }

    fn shared(visualization: &str, second_category: bool) -> Value {
        let mut y_items = vec![json!({"guid": "f_region", "title": "Region", "data_type": "string", "type": "DIMENSION", "id": "dim-1"})];
        if second_category {
            y_items.push(json!({"guid": "f_year", "title": "Year", "data_type": "string", "type": "DIMENSION", "id": "dim-2"}));
        }
        json!({
            "version": "15",
            "datasetsIds": ["demo"],
            "colors": [],
            "labels": [],
            "sort": [],
            "shapes": [],
            "segments": [],
            "tooltips": [{"guid": "f_sales"}],
            "extraSettings": {"labelsPosition": "inside"},
            "visualization": {
                "id": visualization,
                "placeholders": [
                    {"id": "x", "items": [json!({"guid": "f_sales", "title": "Sales", "data_type": "integer", "type": "MEASURE", "id": "msr-1"})]},
                    {"id": "y", "items": y_items},
                ],
            },
        })
    }

    #[test]
    fn sanitizer_normalizes_placeholder_order_and_ignored_settings() {
        let mut config = shared("bar", true);
        sanitize_config(&mut config);
        let placeholders = config.pointer("/visualization/placeholders").and_then(Value::as_array).unwrap();
        assert_eq!(s(&placeholders[0], "id"), "y");
        assert_eq!(s(&placeholders[1], "id"), "x");
        assert!(config.get("tooltips").unwrap().as_array().unwrap().is_empty());
        assert!(config.get("shapes").unwrap().as_array().unwrap().is_empty());
        // the second category becomes the stack/colour dimension
        assert_eq!(array(&config, "colors").len(), 1);
        assert_eq!(s(&array(&config, "colors")[0], "guid"), "f_year");
    }

    #[test]
    fn second_category_builds_stacked_series() {
        let mut config = shared("bar", true);
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("stacked bar data");
        let series = data.pointer("/series/data").and_then(Value::as_array).unwrap();
        assert!(!series.is_empty());
    }

    #[test]
    fn axis_formatting_follows_axis_format_mode() {
        let mut config = shared("bar", false);
        config["visualization"]["placeholders"][0]["settings"] = json!({
            "axisFormatMode": "manual",
            "axisLabelFormating": {"format": "number", "precision": 1},
        });
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("bar data");
        assert_eq!(
            data.pointer("/xAxis/labels/numberFormat"),
            Some(&json!({"format": "number", "precision": 1})),
        );

        let mut config = shared("bar", false);
        config["visualization"]["placeholders"][0]["settings"] =
            json!({"axisFormatMode": "by-field"});
        config["visualization"]["placeholders"][0]["items"][0]["formatting"] =
            json!({"format": "number", "precision": 0, "unit": "m"});
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("bar data");
        assert_eq!(
            data.pointer("/xAxis/labels/numberFormat"),
            Some(&json!({"format": "number", "precision": 0, "unit": "m"})),
        );

        // auto keeps the automatic client format; bar100p is a percent visualization.
        let mut config = shared("bar", false);
        config["visualization"]["placeholders"][0]["settings"] =
            json!({"axisFormatMode": "manual", "axisLabelFormating": {"precision": 1}});
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("bar data");
        assert!(
            data.pointer("/xAxis/labels/numberFormat").is_some(),
            "manual mode must keep the axis numberFormat",
        );

        let mut config = shared("bar100p", false);
        config["visualization"]["placeholders"][0]["settings"] =
            json!({"axisFormatMode": "manual", "axisLabelFormating": {"precision": 1}});
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("bar100p data");
        assert!(
            data.pointer("/xAxis/labels/numberFormat").is_none(),
            "bar100p is a percent visualization",
        );
    }

    #[test]
    fn bar100p_without_colour_builds_percent_series() {
        let mut config = shared("bar100p", false);
        sanitize_config(&mut config);
        let data = build_data(&config, &fetched(), "classic20").expect("bar100p data");
        assert_eq!(
            data.pointer("/series/data/0/stacking").and_then(Value::as_str),
            Some("percent")
        );
    }
}
