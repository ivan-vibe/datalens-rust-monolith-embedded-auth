//! Rust port of the **Gravity scatter** preparer for `graph_wizard_node` charts with
//! `visualization.id == "scatter"`.
//!
//! This covers exactly one rendering path of the wizard worker:
//!
//! * `preparers/scatter/prepare-scatter.js:prepareScatter` – points, labels, radius,
//!   palette grouping;
//! * `preparers/scatter/gravity-charts.js:prepareGravityChartsScatter` – series, axes,
//!   legend, tooltip, chart margins/zoom, `custom` metadata for tooltips/export;
//! * `gravity-charts/utils/index.js:getBaseChartConfig` – the shared gravity chart shell;
//! * `config/index.js:buildChartsConfigPrivate` – the `config` member;
//! * `url/build-sources.js` + `url/build-request-body/*` – the data-api request (delegated
//!   to [`crate::graph_engine::prepare`], which already mirrors them for graph nodes).
//!
//! `highchartsConfig` is `"{}"` for gravity charts (`buildLibraryConfig` returns `{}`).
//!
//! Scope guards – [`prepare`] rejects more complex scatters so the caller can fall back to
//! V8: categories/date axes, logarithmic scales, gradient colouring, shapes,
//! markup/markdown/html fields, multiple colour fields, non-default palettes and manual
//! axis formatting are not ported.
//!
//! Wiring (see `patches/gravity_scatter.md`):
//!
//! ```ignore
//! // main.rs
//! mod gravity_scatter;
//! // pipeline.rs -> run_rust_graph
//! let gravity_scatter = graph_renderer(shared) == GraphRenderer::Gravity
//!     && crate::gravity_scatter::supports(shared);
//! ...
//! let prepared = if gravity_scatter {
//!     crate::gravity_scatter::prepare(shared, params, &datasets)
//! } else {
//!     crate::graph_engine::prepare(shared, params, &datasets)
//! }?;
//! ...
//! let finished = if gravity_scatter {
//!     crate::gravity_scatter::finish(&prepared, &fetched, &state.default_color_palette)
//! } else {
//!     crate::graph_engine::finish(&prepared, &fetched, &state.default_color_palette)
//! }?;
//! ```
//!
//! The envelope is produced by the existing `run_rust_graph`, so `timings.jsExecution` is
//! `0.0` (no JS engine is involved).

use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::graph_engine::{DatasetInfo, GraphFetched, GraphFinished, GraphPrepared};

const MINIMUM_FRACTION_DIGITS: i64 = 2;
const UNITS: [&str; 5] = ["", "K", "M", "B", "T"];
const DEFAULT_MIN_POINT_RADIUS: f64 = 2.0;
const DEFAULT_MAX_POINT_RADIUS: f64 = 8.0;
const DEFAULT_STROKE: &str = "var(--g-color-line-generic)";

/// default20 scheme, the opensource tenant default (`state.default_color_palette`).
const DEFAULT20_SCHEME: [&str; 20] = [
    "#3D97F2", "#FF6A59", "#7FD169", "#FFB433", "#AB94D9", "#8CD5FF", "#FF8CB2", "#60A196",
    "#4C73BF", "#B8C9CC", "#52CCC2", "#C3E55C", "#807D73", "#FF8C00", "#FFDC73", "#CC9966",
    "#CC5252", "#FFB28C", "#9661FF", "#D4BFFF",
];

// ---------------------------------------------------------------------------------------------
// field helpers (the worker's shared modules)
// ---------------------------------------------------------------------------------------------

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn arr<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn data_type(field: &Value) -> &str {
    s(field, "data_type")
}

fn is_number_type(data_type: &str) -> bool {
    matches!(data_type, "integer" | "uinteger" | "float")
}

fn is_number_field(field: &Value) -> bool {
    is_number_type(data_type(field))
}

fn is_date_type(data_type: &str) -> bool {
    matches!(data_type, "date" | "datetime" | "genericdatetime" | "datetimetz")
}

fn is_date_field(field: &Value) -> bool {
    is_date_type(data_type(field))
}

fn is_measure_field(field: &Value) -> bool {
    s(field, "type") == "MEASURE"
}

fn is_measure_value_field(field: &Value) -> bool {
    s(field, "type") == "PSEUDO" && s(field, "title") == "Measure Values"
}

/// `isGradientMode` (`utils/misc-helpers.js`).
fn is_gradient_mode(field: &Value, colors_config: &Value) -> bool {
    if is_measure_field(field) || is_measure_value_field(field) {
        return true;
    }
    is_number_field(field) && s(colors_config, "colorMode") == "gradient"
}

/// System gradient palettes (`shared/constants/gradients/*`).
fn gradient_palette(colors_config: &Value) -> Result<Vec<String>, String> {
    if let Some(colors) = colors_config.get("gradientColors").and_then(Value::as_array) {
        if !colors.is_empty() {
            return Ok(colors.iter().filter_map(Value::as_str).map(str::to_string).collect());
        }
    }
    let palette = s(colors_config, "gradientPalette");
    let colors: Vec<&str> = match palette {
        "red-orange-green" => vec!["#FF3D64", "#FFC636", "#54A520"],
        other => {
            return Err(format!(
                "gravity scatter: gradient palette {other} is not ported"
            ))
        }
    };
    Ok(colors.iter().map(|color| color.to_string()).collect())
}

fn transform_hex_to_rgb(color: &str) -> (i64, i64, i64) {
    let red = i64::from_str_radix(color.get(1..3).unwrap_or("00"), 16).unwrap_or(0);
    let green = i64::from_str_radix(color.get(3..5).unwrap_or("00"), 16).unwrap_or(0);
    let blue = i64::from_str_radix(color.get(5..7).unwrap_or("00"), 16).unwrap_or(0);
    (red, green, blue)
}

fn gradient_thresholds(colors_config: &Value, values: &[Option<f64>]) -> (f64, f64, f64) {
    let numbers: Vec<f64> = values.iter().filter_map(|value| *value).collect();
    let data_min = numbers.iter().copied().fold(f64::INFINITY, f64::min);
    let data_max = numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let manual = s(colors_config, "thresholdsMode") == "manual";
    let max = if manual {
        colors_config
            .get("rightThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(data_max)
    } else {
        data_max
    };
    let min = if manual {
        colors_config
            .get("leftThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(data_min)
    } else {
        data_min
    };
    let range = max - min;
    let three_point = s(colors_config, "gradientMode") == "3-point";
    let mid = if manual && three_point {
        colors_config
            .get("middleThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(max - range / 2.0)
    } else {
        max - range / 2.0
    };
    (min, mid, max)
}

fn range_delta(color_value: Option<f64>, min: f64, range: f64) -> Option<f64> {
    let color_value = color_value?;
    let mut delta = (color_value - min) / range;
    if delta > 1.0 {
        delta = 1.0;
    } else if delta < 0.0 {
        delta = 0.0;
    }
    Some(delta)
}

/// `getColorsByMeasureField` (`utils/color-helpers.js`).
fn gradient_point_colors(
    colors_config: &Value,
    gradient_colors: &[String],
    values: &[Option<f64>],
) -> Vec<Option<String>> {
    let (min, mid, max) = gradient_thresholds(colors_config, values);
    let range = max - min;
    let range_middle = mid - min;
    let range_middle_ratio = range_middle / range;
    let mut rgb: Vec<(i64, i64, i64)> = gradient_colors
        .iter()
        .map(|color| transform_hex_to_rgb(color))
        .collect();
    if colors_config.get("reversed").and_then(Value::as_bool).unwrap_or(false) {
        rgb.reverse();
    }
    let three_point = s(colors_config, "gradientMode") == "3-point" && rgb.len() >= 3;
    values
        .iter()
        .map(|value| {
            let delta = if range == 0.0 {
                Some(0.5)
            } else {
                range_delta(*value, min, range)
            };
            let Some(delta) = delta else { return None };
            let (delta, shade_a, shade_b) = if three_point {
                if delta >= range_middle_ratio {
                    let result_delta = if delta == 1.0 { delta } else { delta - range_middle_ratio };
                    (result_delta, rgb[1], rgb[2])
                } else {
                    (delta, rgb[0], rgb[1])
                }
            } else {
                (delta, rgb[0], rgb[1])
            };
            let red = ((shade_b.0 - shade_a.0) as f64 * delta + shade_a.0 as f64).floor() as i64;
            let green = ((shade_b.1 - shade_a.1) as f64 * delta + shade_a.1 as f64).floor() as i64;
            let blue = ((shade_b.2 - shade_a.2) as f64 * delta + shade_a.2 as f64).floor() as i64;
            Some(format!("rgb({red}, {green}, {blue})"))
        })
        .collect()
}

/// `getLegendColorScale` for a continuous gradient legend.
fn gradient_legend(colors_config: &Value, color_field: &Value, values: &[Option<f64>]) -> Result<Value, String> {
    let gradient_colors = gradient_palette(colors_config)?;
    let mut colors: Vec<String> = gradient_colors.clone();
    if colors_config.get("reversed").and_then(Value::as_bool).unwrap_or(false) {
        colors.reverse();
    }
    // JS `points.map(p => p.colorValue).filter(d => Boolean(d))` – zero values are dropped.
    let visible: Vec<f64> = values
        .iter()
        .filter_map(|value| *value)
        .filter(|value| *value != 0.0)
        .collect();
    let min_color_value = visible.iter().copied().fold(f64::INFINITY, f64::min);
    let max_color_value = visible.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (min, mid, max) = gradient_thresholds(colors_config, values);
    let mut stops: Vec<f64> = if s(colors_config, "gradientMode") == "2-point" || colors.len() == 2 {
        vec![
            (min - min_color_value) / (max_color_value - min_color_value),
            (max - min_color_value) / (max_color_value - min_color_value),
        ]
    } else {
        vec![
            (min - min_color_value) / (max_color_value - min_color_value),
            (mid - min_color_value) / (max_color_value - min_color_value),
            (max - min_color_value) / (max_color_value - min_color_value),
        ]
    };
    if !stops.is_empty() && stops[0] > 0.0 {
        stops.insert(0, 0.0);
        let first = colors.first().cloned().unwrap_or_default();
        colors.insert(0, first);
    }
    if !stops.is_empty() && *stops.last().unwrap_or(&1.0) < 1.0 {
        stops.push(1.0);
        let last = colors.last().cloned().unwrap_or_default();
        colors.push(last);
    }
    Ok(json!({
        "enabled": true,
        "type": "continuous",
        "title": {
            "text": fake_title_or_title(color_field),
            "style": {"fontWeight": "500"}
        },
        "colorScale": {"colors": colors, "stops": stops}
    }))
}

fn is_plain_string_field(field: &Value) -> bool {
    data_type(field) == "string"
}

/// `isMarkupField` / `isHtmlField` / `isMarkdownField` – all unsupported by this port.
fn is_rich_text_field(field: &Value) -> bool {
    let markup_type = s(field, "markupType");
    data_type(field) == "markup" || markup_type == "html" || markup_type == "markdown"
}

fn fake_title_or_title(field: &Value) -> String {
    if field.is_null() {
        return String::new();
    }
    let fake = s(field, "fakeTitle");
    if !fake.is_empty() {
        fake.to_string()
    } else {
        s(field, "title").to_string()
    }
}

fn original_title_or_title(field: &Value) -> String {
    let original = s(field, "originalTitle");
    if !original.is_empty() {
        original.to_string()
    } else {
        s(field, "title").to_string()
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number
            .as_f64()
            .map(|number| number != 0.0 && !number.is_nan())
            .unwrap_or(false),
        // JS: arrays and objects are always truthy.
        _ => true,
    }
}

fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Null => None,
        _ => None,
    }
}

/// JSON number the way `JSON.stringify` writes JS numbers (integers stay integers).
fn json_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
        return json!(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------------------------
// number formatting (`shared/modules/format-units`, `fields.js:getFormatOptions`)
// ---------------------------------------------------------------------------------------------

/// `getFormatOptions` – number fields only.
fn format_options(field: &Value) -> Value {
    if !is_number_field(field) {
        return Value::Null;
    }
    let mut formatting = field
        .get("formatting")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if formatting
        .as_object()
        .map(|object| object.is_empty())
        .unwrap_or(true)
    {
        formatting = s(field, "ui_settings")
            .parse::<Value>()
            .ok()
            .and_then(|settings| settings.pointer("/numberFormatting").cloned())
            .unwrap_or_else(|| json!({}));
    }
    if formatting.get("precision").and_then(Value::as_f64).is_none()
        && data_type(field) == "float"
    {
        if let Some(object) = formatting.as_object_mut() {
            object.insert("precision".to_string(), json!(MINIMUM_FRACTION_DIGITS));
        }
    }
    formatting
}

fn compute_unit_rate(value: f64) -> i64 {
    let mut rate = 1i64;
    while rate < 10
        && UNITS.get(rate as usize).is_some()
        && (value / 1000f64.powi(rate as i32)).abs() >= 1.0
    {
        rate += 1;
    }
    rate - 1
}

fn group_digits(text: &str) -> String {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let mut first_group = digits.len();
    while first_group > 3 {
        first_group -= 3;
    }
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    let mut left = first_group;
    for character in digits.chars() {
        if left == 0 {
            grouped.push('\u{a0}');
            left = 3;
        }
        grouped.push(character);
        left -= 1;
    }
    format!("{sign}{grouped}")
}

use crate::js_format::fixed_half_away;

/// `Intl.NumberFormat("ru-RU", { minimumFractionDigits, maximumFractionDigits: 20 })`.
fn intl_number(value: f64, minimum_fraction_digits: i64, grouping: bool) -> String {
    let text = if minimum_fraction_digits > 0 {
        fixed_half_away(value, minimum_fraction_digits as usize)
    } else {
        format!("{value}")
    };
    let (whole, mut fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole.to_string(), fraction.to_string()),
        None => (text.clone(), String::new()),
    };
    while (fraction.len() as i64) < minimum_fraction_digits {
        fraction.push('0');
    }
    let whole = if grouping {
        group_digits(&whole)
    } else {
        whole
    };
    if fraction.is_empty() {
        whole
    } else {
        format!("{whole},{fraction}")
    }
}

fn js_to_fixed(value: f64, digits: i64) -> f64 {
    fixed_half_away(value, digits.max(0) as usize)
        .parse()
        .unwrap_or(value)
}

/// `formatNumber(value, options)` from `shared/modules/format-units/formatUnit.js`.
fn format_number(value: f64, options: &Value) -> String {
    if value.is_nan() {
        return format!("{value}");
    }
    let format = options
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("number");
    let multiplier = options
        .get("multiplier")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let prefix = options.get("prefix").and_then(Value::as_str).unwrap_or("");
    let postfix = options
        .get("postfix")
        .and_then(Value::as_str)
        .unwrap_or("");
    let label_mode = options.get("labelMode").and_then(Value::as_str);
    let mut changed_multiplier = multiplier;
    let mut pre_postfix = "";
    if format == "percent" {
        changed_multiplier = 100.0;
        pre_postfix = "%";
    }
    if label_mode == Some("percent") {
        pre_postfix = "%";
    }
    let explicit_rate = options.get("unitRate").and_then(Value::as_f64);
    let unit = options.get("unit").and_then(Value::as_str);
    let explicit_unit_rate = match explicit_rate {
        Some(rate) => Some(rate as i64),
        None => match unit {
            Some("auto") => None,
            Some("k") => Some(1),
            Some("m") => Some(2),
            Some("b") => Some(3),
            Some("t") => Some(4),
            _ => Some(0),
        },
    };
    let base = value * changed_multiplier;
    let rate = explicit_unit_rate.unwrap_or_else(|| compute_unit_rate(base));
    let mut result = base / 1000f64.powi(rate as i32);
    let mut minimum_fraction_digits = 0i64;
    match options.get("precision") {
        Some(Value::Number(number)) => {
            if let Some(digits) = number.as_f64() {
                minimum_fraction_digits = digits as i64;
                result = js_to_fixed(result, minimum_fraction_digits);
            }
        }
        Some(Value::String(text)) if text == "auto"
            && result.fract() != 0.0 => {
                let digits = if result.abs() > 1.0 { 2 } else { 4 };
                result = js_to_fixed(result, digits);
            }
        _ => {}
    }
    let grouping = options
        .get("showRankDelimiter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let formatted = intl_number(result, minimum_fraction_digits, grouping);
    let unit_text = UNITS.get(rate as usize).copied().unwrap_or("");
    format!("{prefix}{formatted}{pre_postfix}{unit_text}{postfix}")
}

// ---------------------------------------------------------------------------------------------
// request/data helpers
// ---------------------------------------------------------------------------------------------

fn placeholder<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    placeholders
        .iter()
        .find(|placeholder| s(placeholder, "id") == id)
}

fn placeholder_first_item<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    arr(placeholder(placeholders, id)?, "items").first()
}

fn placeholder_item_at(placeholders: &[Value], index: usize) -> Option<&Value> {
    arr(placeholders.get(index)?, "items").first()
}

fn placeholder_settings<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    placeholder(placeholders, id)?.get("settings")
}

fn axis_title(settings: Option<&Value>, field: &Value) -> Option<String> {
    let settings = settings?;
    match settings.get("title").and_then(Value::as_str) {
        Some("auto") => Some(original_title_or_title(field)),
        Some("manual") => settings
            .get("titleValue")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("off") => None,
        _ => None,
    }
}

fn labels_enabled(settings: Option<&Value>) -> bool {
    settings.and_then(|settings| settings.get("hideLabels")).and_then(Value::as_str) != Some("yes")
}

fn is_grid_enabled(settings: Option<&Value>) -> bool {
    settings.and_then(|settings| settings.get("grid")).and_then(Value::as_str) != Some("off")
}

fn tick_pixel_interval(settings: Option<&Value>) -> Option<f64> {
    let settings = settings?;
    if settings.get("grid").and_then(Value::as_str) != Some("on") {
        return None;
    }
    if settings.get("gridStep").and_then(Value::as_str) != Some("manual") {
        return None;
    }
    settings.get("gridStepValue").and_then(Value::as_f64)
}

/// `getPointRadius` from `utils/misc-helpers.js`.
fn point_radius(current: f64, min: f64, max: f64, geopoints: &Value) -> f64 {
    let min_radius = geopoints
        .get("minRadius")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_MIN_POINT_RADIUS);
    let max_radius = geopoints
        .get("maxRadius")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_MAX_POINT_RADIUS);
    let range = if max == min { 1.0 } else { max - min };
    let coefficient = (max_radius - min_radius) / range;
    min_radius + (current - min) * coefficient
}

fn palette_scheme(default_palette_id: &str) -> Result<Vec<String>, String> {
    if default_palette_id != "default20" {
        return Err(format!(
            "gravity scatter: default palette {default_palette_id} is not ported"
        ));
    }
    Ok(DEFAULT20_SCHEME.iter().map(|color| color.to_string()).collect())
}

// ---------------------------------------------------------------------------------------------
// prepare
// ---------------------------------------------------------------------------------------------

/// True when the chart is a scatter that may be served by this module.
pub fn supports(shared: &Value) -> bool {
    shared
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        == Some("scatter")
}

/// Validate the chart against the ported feature set. Returns `Err` so the caller falls
/// back to V8 for every scatter shape that is not reproduced here.
/// v9: structural checks only. Category/date x axes, manual axis modes, gradient
/// colouring, shapes and markup fields are rendered by the builder below where possible;
/// settings it cannot represent fall back to the classic Highcharts conversion in
/// `run_rust_graph` instead of returning a 501 here.
pub fn check_supported(shared: &Value) -> Result<(), String> {
    let visualization = shared
        .get("visualization")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if s(&visualization, "id") != "scatter" {
        return Err("gravity scatter: visualization is not scatter".to_string());
    }
    let placeholders = arr(&visualization, "placeholders");
    if placeholder_first_item(placeholders, "x").is_none() {
        return Err("gravity scatter: x field is missing".to_string());
    }
    if placeholder_first_item(placeholders, "y").is_none() {
        return Err("gravity scatter: y field is missing".to_string());
    }
    Ok(())
}

/// Normalise visual settings the scatter builder does not render: one colour field,
/// no separate shapes/tooltips config, automatic axis modes, first label/sort only.
fn sanitize_config(config: &mut Value) {
    if let Some(colors) = config.get("colors").and_then(Value::as_array) {
        if colors.len() > 1 {
            config["colors"] = json!([colors[0].clone()]);
        }
    }
    config["shapes"] = json!([]);
    config["tooltips"] = json!([]);
    if let Some(map) = config.as_object_mut() {
        map.remove("tooltipConfig");
    }
    if let Some(extra) = config.get_mut("extraSettings").and_then(Value::as_object_mut) {
        extra.remove("labelsPosition");
    }
    if let Some(placeholders) = config
        .pointer_mut("/visualization/placeholders")
        .and_then(Value::as_array_mut)
    {
        for placeholder in placeholders.iter_mut() {
            if let Some(settings) = placeholder.get_mut("settings").and_then(Value::as_object_mut) {
                settings.remove("axisModeMap");
                if settings.get("type").and_then(Value::as_str) == Some("logarithmic") {
                    settings.remove("type");
                }
            }
        }
    }
}

pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &HashMap<String, DatasetInfo>,
) -> Result<GraphPrepared, String> {
    let mut prepared = crate::graph_engine::prepare(shared, params, datasets)?;
    check_supported(&prepared.config)?;
    if prepared.requests.len() != 1 {
        return Err("gravity scatter: exactly one dataset is expected".to_string());
    }
    sanitize_config(&mut prepared.config);
    Ok(prepared)
}

// ---------------------------------------------------------------------------------------------
// data (`prepareScatter` + `prepareGravityChartsScatter`)
// ---------------------------------------------------------------------------------------------

fn field_positions(
    fields: &[Value],
    legend: &[Value],
) -> HashMap<String, usize> {
    let mut positions = HashMap::new();
    for (position, legend_id) in legend.iter().enumerate() {
        let Some(legend_id) = legend_id.as_i64() else {
            continue;
        };
        let Some(field) = fields
            .iter()
            .find(|field| field.get("legend_item_id").and_then(Value::as_i64) == Some(legend_id))
        else {
            continue;
        };
        let role = field
            .pointer("/role_spec/role")
            .and_then(Value::as_str)
            .unwrap_or("");
        if matches!(role, "order_by" | "filter" | "info" | "parameter" | "total" | "template")
        {
            continue;
        }
        if let Some(id) = field.get("id").and_then(Value::as_str) {
            positions.entry(id.to_string()).or_insert(position);
        }
    }
    positions
}


/// One raw point as produced by `prepareScatter`.
struct ScatterPoint {
    x: Value,
    y: Value,
    x_label: String,
    y_label: String,
    name: Option<String>,
    color_value: Option<Value>,
    color_label: Option<String>,
    size_value: Option<f64>,
    size_label: Option<String>,
    radius: f64,
    color: Option<String>,
}

/// One scatter series of the typed output: the points are kept once in
/// [`ScatterData::points`]; a series only stores the indices it owns, so a palette chart with
/// N series does not clone every point N times (the old `Value` builder did).
struct ScatterSeries {
    name: String,
    color: Option<String>,
    custom: Value,
    indices: Vec<usize>,
}

/// The scatter `data` member, built directly and serialized once into the envelope.
///
/// v9.5: this replaces the intermediate `Value` tree (per-point `Map`s with allocated string
/// keys, then cloned `x`/`y` values). The envelope writes this struct straight into its buffer,
/// so a 1.2 MB scatter response no longer materializes a second 1.2 MB JSON tree.
pub struct ScatterData {
    tooltip_enabled: bool,
    legend: Value,
    series: Vec<ScatterSeries>,
    points: Vec<ScatterPoint>,
    x_axis: Value,
    y_axis: Value,
}

impl ScatterData {
    /// The `Value` shape, for tests and the few call sites that still need a tree.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `{data: [...], options: {...}}`
struct ScatterSeriesField<'a> {
    series: &'a [ScatterSeries],
    points: &'a [ScatterPoint],
}

impl serde::Serialize for ScatterSeriesField<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(
            "data",
            &ScatterSeriesList {
                series: self.series,
                points: self.points,
            },
        )?;
        map.serialize_entry(
            "options",
            &json!({
                "bar-x": {
                    "barMaxWidth": 50,
                    "barPadding": 0.05,
                    "groupPadding": 0.4,
                    "dataSorting": {"direction": "desc", "key": "name"},
                },
                "line": {"lineWidth": 2},
                "area": {"lineWidth": 2},
            }),
        )?;
        map.end()
    }
}

struct ScatterSeriesList<'a> {
    series: &'a [ScatterSeries],
    points: &'a [ScatterPoint],
}

impl serde::Serialize for ScatterSeriesList<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut list = serializer.serialize_seq(Some(self.series.len()))?;
        for series in self.series {
            list.serialize_element(&ScatterSeriesOut {
                series,
                points: self.points,
            })?;
        }
        list.end()
    }
}

struct ScatterSeriesOut<'a> {
    series: &'a ScatterSeries,
    points: &'a [ScatterPoint],
}

impl serde::Serialize for ScatterSeriesOut<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("type", "scatter")?;
        map.serialize_entry("name", &self.series.name)?;
        if let Some(color) = &self.series.color {
            map.serialize_entry("color", color)?;
        }
        map.serialize_entry(
            "data",
            &ScatterPointsOut {
                points: self.points,
                indices: &self.series.indices,
            },
        )?;
        map.serialize_entry("custom", &self.series.custom)?;
        map.serialize_entry("symbolType", "circle")?;
        map.end()
    }
}

struct ScatterPointsOut<'a> {
    points: &'a [ScatterPoint],
    indices: &'a [usize],
}

impl serde::Serialize for ScatterPointsOut<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut list = serializer.serialize_seq(Some(self.indices.len()))?;
        for index in self.indices {
            list.serialize_element(&ScatterPointOut(&self.points[*index]))?;
        }
        list.end()
    }
}

/// A number serialized exactly like [`json_number`] would turn it into a `Value`:
/// integral finite values become JSON integers, everything else a JSON float, non-finite null.
struct JsonNumberOut(f64);

impl serde::Serialize for JsonNumberOut {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = self.0;
        if !value.is_finite() {
            return serializer.serialize_unit();
        }
        if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_992.0 {
            return serializer.serialize_i64(value as i64);
        }
        serializer.serialize_f64(value)
    }
}

/// One point: `{radius, custom: {...}, color?, x, y}` serialized without building a `Value`.
struct ScatterPointOut<'a>(&'a ScatterPoint);

impl serde::Serialize for ScatterPointOut<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let point = self.0;
        let mut map = serializer.serialize_map(Some(5))?;
        map.serialize_entry("radius", &JsonNumberOut(point.radius))?;
        map.serialize_entry("custom", &ScatterCustomOut(point))?;
        if let Some(color) = &point.color {
            map.serialize_entry("color", color)?;
        }
        map.serialize_entry("x", &point.x)?;
        map.serialize_entry("y", &point.y)?;
        map.end()
    }
}

struct ScatterCustomOut<'a>(&'a ScatterPoint);

impl serde::Serialize for ScatterCustomOut<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let point = self.0;
        let mut map = serializer.serialize_map(Some(7))?;
        if let Some(name) = &point.name {
            map.serialize_entry("name", name)?;
        }
        map.serialize_entry("xLabel", &point.x_label)?;
        map.serialize_entry("yLabel", &point.y_label)?;
        if let Some(color_value) = &point.color_value {
            map.serialize_entry("colorValue", color_value)?;
        }
        if let Some(color_label) = &point.color_label {
            map.serialize_entry("cLabel", color_label)?;
        }
        if let Some(size_value) = point.size_value {
            map.serialize_entry("sizeValue", &JsonNumberOut(size_value))?;
        }
        if let Some(size_label) = &point.size_label {
            map.serialize_entry("sizeLabel", size_label)?;
        }
        map.end()
    }
}

impl serde::Serialize for ScatterData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("tooltip", &json!({"enabled": self.tooltip_enabled}))?;
        map.serialize_entry("legend", &self.legend)?;
        map.serialize_entry(
            "series",
            &ScatterSeriesField {
                series: &self.series,
                points: &self.points,
            },
        )?;
        map.serialize_entry(
            "chart",
            &json!({
                "margin": {"top": 10, "left": 10, "right": 10, "bottom": 15},
                "zoom": {
                    "enabled": true,
                    "resetButton": {
                        "align": "top-right",
                        "offset": {"x": 2, "y": 30},
                        "relativeTo": "plot-box",
                    },
                },
            }),
        )?;
        map.serialize_entry("xAxis", &self.x_axis)?;
        map.serialize_entry("yAxis", &json!([self.y_axis]))?;
        map.end()
    }
}

pub fn build_data(
    config: &Value,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<ScatterData, String> {
    let response = fetched
        .first()
        .map(|fetched| &fetched.body)
        .ok_or_else(|| "gravity scatter: data-api response is missing".to_string())?;
    let visualization = config
        .get("visualization")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let placeholders = arr(&visualization, "placeholders");
    let visualization_id = s(&visualization, "id").to_string();
    let x_field = placeholder_first_item(placeholders, "x")
        .ok_or_else(|| "gravity scatter: x field is missing".to_string())?;
    let y_field = placeholder_first_item(placeholders, "y")
        .ok_or_else(|| "gravity scatter: y field is missing".to_string())?;
    let z_field = placeholder_item_at(placeholders, 2);
    let size_field = placeholder_item_at(placeholders, 3);
    let color_field = arr(config, "colors").first();
    let colors_config = config
        .get("colorsConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let gradient_mode = color_field
        .map(|field| is_gradient_mode(field, &colors_config))
        .unwrap_or(false);

    // Borrow the response arrays: the old code deep-cloned `fields`, **every row** and the
    // legend before iterating them, so a 1.2 MB result was cloned once up front and then each
    // cell was cloned again into the point structs.
    let no_fields: &[Value] = &[];
    let fields: &[Value] = response
        .get("fields")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_fields);
    let no_rows: &[Value] = &[];
    let rows: &[Value] = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_rows);
    let Some(first_row) = rows.first() else {
        return Err("gravity scatter: data-api returned no rows".to_string());
    };
    let no_legend: &[Value] = &[];
    let legend: &[Value] = first_row
        .get("legend")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_legend);
    let positions = field_positions(fields, legend);
    let column_index = |field: &Value| -> Result<usize, String> {
        positions
            .get(s(field, "guid"))
            .copied()
            .ok_or_else(|| format!("gravity scatter: {} is missing in the result", s(field, "guid")))
    };
    let row_value = |row: &Value, index: usize| -> Value {
        row.get("data")
            .and_then(Value::as_array)
            .and_then(|data| data.get(index))
            .cloned()
            .unwrap_or(Value::Null)
    };

    let x_index = column_index(x_field)?;
    let y_index = column_index(y_field)?;
    let z_index = z_field.map(column_index).transpose()?;
    let size_index = size_field.map(column_index).transpose()?;
    let color_index = color_field.map(column_index).transpose()?;

    // `getExtremeValues` over every row (before the x-null skip).
    let mut size_min: Option<f64> = None;
    let mut size_max: Option<f64> = None;
    if let Some(index) = size_index {
        for row in rows {
            let Some(value) = js_number(&row_value(row, index)) else {
                continue;
            };
            size_min = Some(size_min.map(|min| min.min(value)).unwrap_or(value));
            size_max = Some(size_max.map(|max| max.max(value)).unwrap_or(value));
        }
    }

    let x_is_category = !is_number_field(x_field) && !is_date_field(x_field);
    let mut x_categories: Vec<String> = Vec::new();
    let mut x_category_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // `prepare-scatter.js` builds `yCategories` the same way as `xCategories` and replaces a
    // non-numeric y with its first-appearance index; the axis becomes a category axis. Without
    // this a text Y (e.g. Client ID) was sent as a huge raw number and the wizard chart drew
    // "1 000 000T" instead of the category axis.
    let y_is_category = !is_number_field(y_field) && !is_date_field(y_field);
    let mut y_categories: Vec<String> = Vec::new();
    let mut y_category_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let x_formatting = format_options(x_field);
    let y_formatting = format_options(y_field);
    let size_formatting = size_field.map(format_options);
    let geopoints = config
        .get("geopointsConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Formatting options are the same for every row: hoist them out of the loop (the old
    // gradient branch rebuilt `format_options` for the colour field on every single point).
    let size_options = size_formatting.clone().unwrap_or_else(|| json!({}));
    let gradient_color_options = if gradient_mode {
        let field = color_field.expect("gradient mode has a colour field");
        let formatting = format_options(field);
        if formatting
            .as_object()
            .map(|object| object.is_empty())
            .unwrap_or(true)
        {
            json!({"precision": if data_type(field) == "float" { MINIMUM_FRACTION_DIGITS } else { 0 }})
        } else {
            formatting
        }
    } else {
        Value::Null
    };
    let mut points: Vec<ScatterPoint> = Vec::with_capacity(rows.len());
    for row in rows {
        let x_raw = row_value(row, x_index);
        if x_raw.is_null() {
            continue;
        }
        let (x_value, x_label) = if x_is_category {
            // v9: categorical x — the axis is a category axis, so points carry the
            // first-appearance index and the category label.
            let label = x_raw
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| x_raw.to_string());
            let index = *x_category_index.entry(label.clone()).or_insert_with(|| {
                let index = x_categories.len();
                x_categories.push(label.clone());
                index
            });
            (index as f64, label)
        } else if is_date_field(x_field) {
            // `formatDate({valueType, value, format, utc: true})` on the parsed millis.
            let millis = crate::gravity_donut::date_millis(&x_raw);
            let value = millis.map(|millis| millis as f64).unwrap_or(0.0);
            (
                value,
                crate::gravity_donut::format_date(&x_raw, data_type(x_field), s(x_field, "format")),
            )
        } else {
            let value = js_number(&x_raw).unwrap_or(0.0);
            (value, format_number(value, &x_formatting))
        };
        let y_raw = row_value(row, y_index);
        let (y_value, y_label) = if y_is_category {
            let label = y_raw
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| y_raw.to_string());
            let index = *y_category_index.entry(label.clone()).or_insert_with(|| {
                let index = y_categories.len();
                y_categories.push(label.clone());
                index
            });
            (index as f64, label)
        } else if is_date_field(y_field) {
            let millis = crate::gravity_donut::date_millis(&y_raw);
            let value = millis.map(|millis| millis as f64).unwrap_or(0.0);
            (
                value,
                crate::gravity_donut::format_date(&y_raw, data_type(y_field), s(y_field, "format")),
            )
        } else {
            let value = js_number(&y_raw).unwrap_or(0.0);
            (value, format_number(value, &y_formatting))
        };
        let mut point = ScatterPoint {
            x: json_number(x_value),
            y: json_number(y_value),
            x_label,
            y_label,
            name: None,
            color_value: None,
            color_label: None,
            size_value: None,
            size_label: None,
            radius: geopoints
                .get("radius")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            color: None,
        };
        if let Some(index) = z_index {
            let raw = row_value(row, index);
            let formatted = if is_number_field(z_field.unwrap()) {
                js_number(&raw)
                    .map(|value| format_number(value, &format_options(z_field.unwrap())))
                    .unwrap_or_default()
            } else {
                raw.as_str().unwrap_or("").to_string()
            };
            point.name = Some(formatted);
        }
        if let (Some(index), Some(_)) = (size_index, size_field) {
            let value = js_number(&row_value(row, index)).unwrap_or(0.0);
            point.size_value = Some(value);
            point.size_label = Some(format_number(value, &size_options));
            let (min, max) = (size_min.unwrap_or(value), size_max.unwrap_or(value));
            point.radius = point_radius(value, min, max, &geopoints);
        }
        if let (Some(index), Some(_field)) = (color_index, color_field) {
            let raw = row_value(row, index);
            if gradient_mode {
                let number = js_number(&raw).unwrap_or(f64::NAN);
                point.color_value = Some(json_number(number));
                point.color_label = Some(format_number(number, &gradient_color_options));
            } else {
                let label = raw
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| raw.to_string());
                point.color_value = Some(raw);
                point.color_label = Some(label);
            }
        }
        points.push(point);
    }

    let series_custom = series_custom(x_field, y_field, z_field, size_field, color_field);
    if gradient_mode {
        // `mapAndColorizePointsByGradient`: a single series, continuous colour legend.
        let field = color_field.unwrap();
        let gradient_colors = gradient_palette(&colors_config)?;
        let color_values: Vec<Option<f64>> = points
            .iter()
            .map(|point| point.color_value.as_ref().and_then(js_number))
            .collect();
        let point_colors = gradient_point_colors(&colors_config, &gradient_colors, &color_values);
        for (point, color) in points.iter_mut().zip(point_colors) {
            point.color = color;
        }
        let legend = gradient_legend(&colors_config, field, &color_values)?;
        let tooltip_enabled = config
            .get("extraSettings")
            .map(|extra| s(extra, "tooltip") != "hide")
            .unwrap_or(true);
        let indices = (0..points.len()).collect();
        return Ok(ScatterData {
            tooltip_enabled,
            legend,
            series: vec![ScatterSeries {
                name: String::new(),
                color: None,
                custom: series_custom,
                indices,
            }],
            points,
            x_axis: x_axis(
                placeholders,
                x_field,
                if x_is_category { Some(x_categories.as_slice()) } else { None },
                &visualization_id,
            )?,
            y_axis: y_axis(placeholders, y_field, &visualization_id),
        });
    }

    // `mapAndColorizePointsByPalette`: one series per colour value, in first-appearance order.
    let palette = palette_scheme(default_palette_id)?;
    let mut series: Vec<(Value, Vec<usize>)> = Vec::new();
    for (index, point) in points.iter().enumerate() {
        let value = point.color_value.clone().unwrap_or(Value::Null);
        if let Some(position) = series.iter().position(|(existing, _)| existing == &value) {
            series[position].1.push(index);
        } else {
            series.push((value, vec![index]));
        }
    }

    // `mapScatterSeries` + `seriesCustomData`: each series keeps the indices of the points it
    // owns; the points themselves live once in `ScatterData`.
    let series_data: Vec<ScatterSeries> = series
        .iter()
        .enumerate()
        .map(|(position, (value, point_indices))| ScatterSeries {
            name: value.as_str().map(str::to_string).unwrap_or_default(),
            color: Some(
                palette
                    .get(position % palette.len())
                    .cloned()
                    .unwrap_or_default(),
            ),
            custom: series_custom.clone(),
            indices: point_indices.clone(),
        })
        .collect();

    let legend = if series_data.len() <= 1 {
        json!({"html": false, "enabled": false})
    } else {
        json!({"html": false})
    };
    let tooltip_enabled = config
        .get("extraSettings")
        .map(|extra| s(extra, "tooltip") != "hide")
        .unwrap_or(true);
    Ok(ScatterData {
        tooltip_enabled,
        legend,
        series: series_data,
        points,
        x_axis: x_axis(
            placeholders,
            x_field,
            if x_is_category { Some(x_categories.as_slice()) } else { None },
            &visualization_id,
        )?,
        y_axis: y_axis(placeholders, y_field, &visualization_id),
    })
}

fn series_custom(
    x: &Value,
    y: &Value,
    z: Option<&Value>,
    size: Option<&Value>,
    color: Option<&Value>,
) -> Value {
    let mut columns: Vec<Value> = Vec::new();
    columns.push(export_column("x", x));
    columns.push(export_column("y", y));
    if let Some(z) = z {
        columns.push(export_column("custom.name", z));
    }
    if let Some(size) = size {
        columns.push(export_column("custom.sizeValue", size));
    }
    if let Some(color) = color {
        columns.push(export_column("custom.colorValue", color));
    }
    json!({
        "xTitle": fake_title_or_title(x),
        "yTitle": fake_title_or_title(y),
        "pointTitle": fake_title_or_title(z.unwrap_or(&Value::Null)),
        "colorTitle": fake_title_or_title(color.unwrap_or(&Value::Null)),
        "sizeTitle": fake_title_or_title(size.unwrap_or(&Value::Null)),
        "shapeTitle": "",
        "exportSettings": {"columns": columns},
    })
}

/// `getExportColumnSettings`.
fn export_column(path: &str, field: &Value) -> Value {
    let (column_type, formatter) = if is_number_field(field) {
        ("number", format_options(field))
    } else if is_date_field(field) {
        (data_type(field), json!({}))
    } else {
        ("text", json!({}))
    };
    let mut column = Map::new();
    column.insert("name".to_string(), json!(fake_title_or_title(field)));
    column.insert("formatter".to_string(), formatter);
    if is_date_field(field) {
        let format = {
            let format = s(field, "format");
            if !format.is_empty() {
                format.to_string()
            } else if matches!(data_type(field), "datetime" | "genericdatetime") {
                "DD.MM.YYYY HH:mm:ss".to_string()
            } else {
                "DD.MM.YYYY".to_string()
            }
        };
        column.insert("format".to_string(), json!(format));
    }
    column.insert("field".to_string(), json!(path));
    column.insert("type".to_string(), json!(column_type));
    Value::Object(column)
}

fn x_axis(
    placeholders: &[Value],
    x_field: &Value,
    categories: Option<&[String]>,
    visualization_id: &str,
) -> Result<Value, String> {
    let settings = placeholder_settings(placeholders, "x");
    let axis_type = if is_date_field(x_field) {
        "datetime"
    } else if is_number_field(x_field) {
        if settings
            .and_then(|settings| settings.get("type"))
            .and_then(Value::as_str)
            == Some("logarithmic")
        {
            "logarithmic"
        } else {
            "linear"
        }
    } else {
        "category"
    };
    let mut axis = Map::new();
    axis.insert(
        "visible".to_string(),
        json!(settings
            .and_then(|settings| settings.get("axisVisibility"))
            .and_then(Value::as_str)
            != Some("hide")),
    );
    let mut labels = Map::new();
    labels.insert("enabled".to_string(), json!(labels_enabled(settings)));
    if let Some(formatting) =
        crate::gravity_bar::axis_formatting(placeholder(placeholders, "x"), visualization_id)
    {
        labels.insert("numberFormat".to_string(), formatting);
    }
    axis.insert("labels".to_string(), Value::Object(labels));
    axis.insert(
        "title".to_string(),
        match axis_title(settings, x_field) {
            Some(title) => json!({"text": title}),
            None => json!({}),
        },
    );
    axis.insert("grid".to_string(), json!({"enabled": is_grid_enabled(settings)}));
    let mut ticks = Map::new();
    if let Some(interval) = tick_pixel_interval(settings) {
        ticks.insert("pixelInterval".to_string(), json_number(interval));
    }
    axis.insert("ticks".to_string(), Value::Object(ticks));
    axis.insert("lineColor".to_string(), json!(DEFAULT_STROKE));
    axis.insert("type".to_string(), json!(axis_type));
    if axis_type == "category" {
        axis.insert(
            "categories".to_string(),
            json!(categories.unwrap_or(&[])),
        );
    }
    Ok(Value::Object(axis))
}

fn y_axis(placeholders: &[Value], y_field: &Value, visualization_id: &str) -> Value {
    let settings = placeholder_settings(placeholders, "y");
    let axis_type = if is_date_field(y_field) { "datetime" } else { "linear" };
    let mut axis = Map::new();
    axis.insert("type".to_string(), json!(axis_type));
    axis.insert(
        "visible".to_string(),
        json!(settings
            .and_then(|settings| settings.get("axisVisibility"))
            .and_then(Value::as_str)
            != Some("hide")),
    );
    let mut labels = Map::new();
    labels.insert("enabled".to_string(), json!(labels_enabled(settings)));
    labels.insert("html".to_string(), json!(false));
    if let Some(formatting) =
        crate::gravity_bar::axis_formatting(placeholder(placeholders, "y"), visualization_id)
    {
        labels.insert("numberFormat".to_string(), formatting);
    }
    axis.insert("labels".to_string(), Value::Object(labels));
    axis.insert(
        "title".to_string(),
        match axis_title(settings, y_field) {
            Some(title) => json!({"text": title}),
            None => json!({}),
        },
    );
    axis.insert("grid".to_string(), json!({"enabled": is_grid_enabled(settings)}));
    let mut ticks = Map::new();
    ticks.insert(
        "pixelInterval".to_string(),
        tick_pixel_interval(settings)
            .map(json_number)
            .unwrap_or_else(|| json_number(72.0)),
    );
    axis.insert("ticks".to_string(), Value::Object(ticks));
    axis.insert("lineColor".to_string(), json!("transparent"));
    axis.insert("maxPadding".to_string(), json!(0));
    Value::Object(axis)
}

// ---------------------------------------------------------------------------------------------
// config (`config/index.js:buildChartsConfigPrivate`)
// ---------------------------------------------------------------------------------------------

pub fn build_config(config: &Value, params: &Value) -> Value {
    let extra = config
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut result = Map::new();
    let title = s(&extra, "title");
    if !title.is_empty() && s(&extra, "titleMode") == "show" {
        result.insert("title".to_string(), json!(title));
    }
    result.insert("hideHolidaysBands".to_string(), json!(true));
    result.insert("linesLimit".to_string(), json!(100));
    result.insert(
        "tooltip".to_string(),
        json!({"pin": {"altKey": true}, "sort": {"enabled": true}}),
    );
    result.insert("preventDefaultForPointClick".to_string(), json!(false));
    let mut matched: Vec<Value> = Vec::new();
    if let Some(fields_by_dataset) = config.get("datasetsPartialFields").and_then(Value::as_array) {
        for fields in fields_by_dataset.iter().filter_map(Value::as_array) {
            for field in fields {
                let title = s(field, "title");
                if !title.is_empty() && params.get(title).map(js_truthy).unwrap_or(false) {
                    matched.push(json!(title));
                }
            }
        }
    }
    let mut comments = Map::new();
    comments.insert("matchType".to_string(), json!("intersection"));
    comments.insert("matchedParams".to_string(), Value::Array(matched.clone()));
    let feed = s(&extra, "feed");
    if !feed.is_empty() {
        comments.insert(
            "feeds".to_string(),
            json!([{"feed": feed, "matchedParams": matched, "matchType": "intersection"}]),
        );
    }
    result.insert("comments".to_string(), Value::Object(comments));
    Value::Object(result)
}

// ---------------------------------------------------------------------------------------------
// finish
// ---------------------------------------------------------------------------------------------

/// Build the Gravity scatter `data`/`config`/`sources` for one data-api response.
///
/// The returned shape is [`graph_engine::GraphFinished`] (the same struct the chartkit
/// renderer uses), so `run_rust_graph` can keep one `finished.sources/data/config/library`
/// envelope path for both renderers.
pub fn finish(
    prepared: &GraphPrepared,
    fetched: &[GraphFetched],
    default_palette_id: &str,
) -> Result<GraphFinished, String> {
    let data = build_data(&prepared.config, fetched, default_palette_id)?;
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
        data: crate::graph_engine::GraphData::Scatter(data),
        config,
        library: json!({}),
        sources: Value::Object(sources),
    })
}

/// Convenience entry point (offline parity harness / tests).
pub fn run(
    shared: &Value,
    params: &Value,
    datasets: &HashMap<String, DatasetInfo>,
    fetched: &[(String, Value, u16, f64, u64, Option<Value>)],
    default_palette_id: &str,
) -> Result<(GraphPrepared, GraphFinished), String> {
    let prepared = prepare(shared, params, datasets)?;
    let mut responses = Vec::new();
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
    let finished = finish(&prepared, &responses, default_palette_id)?;
    Ok((prepared, finished))
}

#[cfg(test)]
mod parity {
    use super::*;

    fn fixtures() -> Option<std::path::PathBuf> {
        let path = std::env::var("SCATTER_FIXTURES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/scatter_fx"));
        if path.join("shared.json").exists() {
            Some(path)
        } else {
            None
        }
    }

    fn read(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    #[test]
    fn scatter_16yxd7gojiacq_matches_v8() {
        let Some(dir) = fixtures() else {
            eprintln!("scatter fixtures are missing, skipping");
            return;
        };
        let shared = read(&dir.join("shared.json"));
        // `run_rust_graph` receives `normalized_params` (every value wrapped in an array);
        // the captured body stores raw values, so normalise here the same way.
        let raw_params = read(&dir.join("params.json"));
        let mut params = serde_json::Map::new();
        if let Some(object) = raw_params.as_object() {
            for (key, value) in object {
                params.insert(
                    key.clone(),
                    if value.is_array() { value.clone() } else { json!([value]) },
                );
            }
        }
        let params = Value::Object(params);
        let dataset = read(&dir.join("dataset.json"));
        let expected = read(&dir.join("v8.json"));
        let expected_request = read(&dir.join("v8req.json"));
        let data_api = read(&dir.join("dataapi.json"));
        let mut datasets = HashMap::new();
        datasets.insert(
            "qvnkqz8s0wdqf".to_string(),
            DatasetInfo {
                fields: std::sync::Arc::new(
                    dataset.get("fields").cloned().unwrap_or_else(|| json!([])),
                ),
                revision: std::sync::Arc::new(
                    dataset.get("revision_id").cloned().unwrap_or(Value::Null),
                ),
            },
        );
        let prepared = prepare(&shared, &params, &datasets).expect("prepare");
        assert_eq!(prepared.requests[0].body, expected_request, "request body");
        let fetched = vec![GraphFetched {
            key: prepared.requests[0].key.clone(),
            dataset: "qvnkqz8s0wdqf".to_string(),
            status: 200,
            latency: 0.0,
            size: 0,
            body: data_api,
            data_export: None,
        }];
        let finished = finish(&prepared, &fetched, "default20").expect("finish");
        assert_eq!(
            serde_json::to_string(&finished.data).unwrap(),
            serde_json::to_string(&expected["data"]).unwrap(),
            "data"
        );
        assert_eq!(
            serde_json::to_string(&finished.config).unwrap(),
            expected["config"].as_str().unwrap_or(""),
            "config"
        );
        assert_eq!(
            serde_json::to_string(&finished.library).unwrap(),
            expected["highchartsConfig"].as_str().unwrap_or(""),
            "highchartsConfig"
        );
    }
}


#[cfg(test)]
mod v9_tests {
    use super::*;

    fn category_placeholders() -> Vec<Value> {
        vec![json!({
            "id": "x",
            "settings": {},
            "items": [{"guid": "f_region", "title": "Region", "data_type": "string", "type": "DIMENSION", "id": "dim-1"}],
        })]
    }

    #[test]
    fn categorical_x_builds_category_axis() {
        let placeholders = category_placeholders();
        let field = &placeholders[0]["items"][0];
        let categories = vec!["East".to_string(), "North".to_string()];
        let axis = x_axis(&placeholders, field, Some(&categories), "scatter").expect("category axis");
        assert_eq!(axis.get("type").and_then(Value::as_str), Some("category"));
        assert_eq!(
            axis.get("categories").and_then(Value::as_array).map(Vec::len),
            Some(2)
        );
    }
}
