//! Rust pivot-table engine (`table_wizard_node` with `visualization.id == "pivotTable"`).
//!
//! Ports the two JavaScript stages the original charts-engine runs for a
//! backend pivot table:
//!
//!   * `url/build-request-body` — `prepareSingleRequest` + `buildPivotRequest`
//!     (placeholders -> `fields`, `pivot.structure`, filters, `order_by`,
//!     `parameter_values`, `pivot_pagination`, `updates`,
//!     `dataset_revision_id`), `bundle/urls.js`;
//!   * `preparers/backend-pivot-table` — `head` / `rows` / `footer`
//!     generation, column/row sort metadata, date/number formatting and
//!     color-annotation gradients, `bundle/worker.sync.js`.
//!
//! Plus the pieces shared with the flat-table engine: `buildChartsConfigPrivate`
//! for tables (`config`), `buildGraphPrivate`'s `datasets` extra and the
//! `newParams` expansion, and the `/api/run` envelope with
//! `highchartsConfig: "{}"` and `timings.jsExecution: 0`.
//!
//! Everything here is pure (no I/O, no V8): the caller fetches
//! `GET /api/data/v2/datasets/{id}/fields` for the revision, posts the
//! [`build_request`] body to `POST /api/data/v2/datasets/{id}/pivot`, then
//! hands the decoded response to [`build_data`] / [`build_config`] /
//! [`build_extra`] / [`build_envelope`].
//!
//! Supported subset (everything else returns `Err`, so the caller keeps the
//! JavaScript fallback):
//!
//!   * one dataset; `layers: []` and `links` of that dataset are ignored, a
//!     non-empty layer list or a second dataset (`mergeData`) is rejected;
//!     segments, shapes, labels and tooltips are rejected;
//!   * `pivot-table-columns` / `rows` / `measures` placeholders with regular
//!     dimension/measure fields, several measures, and `Measure Names` (sent as
//!     `ref: {type: "measure_name"}`, the backend synthesizes the pseudo-column);
//!     no tree fields, no parameters in the placeholders;
//!   * drill-down hierarchies (`preprocessHierarchies`): the hierarchy item is
//!     replaced by its field for the current `drillDownLevel`, the applied levels
//!     become `isDrillDown` `EQ` filters, and `config.drillDown` carries the
//!     breadcrumbs and the server date format;
//!   * sub-totals and grand totals: `pivot.totals` levels from
//!     `subTotalsSettings.enabled` (`utils/pivot-table/totals.ts`), placeholder
//!     (`item_type: "placeholder"`) total cells, `isTotalCell`/totals styling,
//!     the `label_total` labels (`helpers/totals.ts`, `ru`/`en`) and the trailing
//!     total rows moved to `footer`;
//!   * per-field `barsSettings` (one-color / two-color / gradient, auto or manual
//!     scale) and `backgroundSettings` (continuous gradients and discrete
//!     `paletteState` maps) for row value cells; chart-wide `shared.colors`
//!     annotations are supported too;
//!   * numeric / string / markup / date cells; `number` and `percent` number
//!     formatting; the default date masks.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Constants (ui-dist copies)
// ---------------------------------------------------------------------------

const TABLE_TOTALS_BG: &str = "var(--g-color-base-generic)";
const DEFAULT_DATE_FORMAT: &str = "DD.MM.YYYY";
const DEFAULT_DATETIME_FORMAT: &str = "DD.MM.YYYY HH:mm:ss";
const DEFAULT_CHART_LINES_LIMIT: u64 = 100;
const TABLE_UNITS: [&str; 5] = ["", "K", "M", "B", "T"];
const MEASURE_NAMES: &str = "Measure Names";
const MEASURE_NAME_PSEUDO_ID: &str = "__@measure_name__";

const THREE_POINT_GRADIENTS: &[(&str, [&str; 3])] = &[
    ("blue-gray-red", ["#0044A3", "#D8D8D8", "#BA2534"]),
    ("blue-yellow-red", ["#1B6DD1", "#FAAF1A", "#F23630"]),
    ("orange-blue-green", ["#FC7E6A", "#8CCBFF", "#A5ED8E"]),
    ("orange-gray-blue", ["#FF9580", "#D8D8D8", "#8CCBFF"]),
    ("orange-violet-blue", ["#FC7E6A", "#6B32C9", "#8CCBFF"]),
    ("pink-gray-green", ["#D0A3FF", "#D8D8D8", "#8EEDCB"]),
    ("red-orange-green", ["#FF3D64", "#FFC636", "#54A520"]),
    ("traffic-light", ["#9C0125", "#FDCE34", "#228782"]),
    ("violet-blue-green", ["#6B32C9", "#57ABFF", "#79E0BF"]),
    ("yellow-green-blue", ["#FFC533", "#64D1B4", "#69B6FF"]),
];

const TWO_POINT_GRADIENTS: &[(&str, [&str; 2])] = &[
    ("blue", ["#0044A3", "#8CCBFF"]),
    ("cyan", ["#1C666E", "#C2EEBA"]),
    ("golden", ["#880132", "#FDEC4D"]),
    ("gray", ["#EAEAEA", "#595959"]),
    ("green", ["#0E752E", "#A5ED8E"]),
    ("green-blue", ["#A5ED8E", "#0044A3"]),
    ("oceanic", ["#050189", "#AEF8FE"]),
    ("orange-yellow", ["#FA5146", "#FFE366"]),
    ("red", ["#BA2534", "#FF9580"]),
    ("red-blue", ["#BA2534", "#1B6DD1"]),
    ("violet", ["#6B32C9", "#D0A3FF"]),
    ("violet-orange", ["#6B32C9", "#FF9580"]),
    ("yellow", ["#944710", "#FFE366"]),
];

fn gradient_colors(gradient_mode: &str, gradient_id: &str) -> Vec<String> {
    if gradient_mode == "3-point" {
        THREE_POINT_GRADIENTS
            .iter()
            .find(|(id, _)| *id == gradient_id)
            .map(|(_, colors)| colors.iter().map(|color| color.to_string()).collect())
            .unwrap_or_else(|| {
                vec![
                    "#FF3D64".to_string(),
                    "#FFC636".to_string(),
                    "#54A520".to_string(),
                ]
            })
    } else {
        TWO_POINT_GRADIENTS
            .iter()
            .find(|(id, _)| *id == gradient_id)
            .map(|(_, colors)| colors.iter().map(|color| color.to_string()).collect())
            .unwrap_or_else(|| vec!["#0044A3".to_string(), "#8CCBFF".to_string()])
    }
}

// ---------------------------------------------------------------------------
// Small JavaScript-value helpers
// ---------------------------------------------------------------------------

/// `field.guid || field.id`
fn field_id(field: &Value) -> Option<&str> {
    field
        .get("guid")
        .and_then(Value::as_str)
        .or_else(|| field.get("id").and_then(Value::as_str))
}

fn field_data_type(field: &Value) -> &str {
    field.get("data_type").and_then(Value::as_str).unwrap_or("")
}

fn is_parameter(field: &Value) -> bool {
    field.get("calc_mode").and_then(Value::as_str) == Some("parameter")
}

fn is_measure_field(field: &Value) -> bool {
    field.get("type").and_then(Value::as_str) == Some("MEASURE")
}

fn is_dimension_field(field: &Value) -> bool {
    field.get("type").and_then(Value::as_str) == Some("DIMENSION") && !is_parameter(field)
}

fn is_pseudo_field(field: &Value) -> bool {
    field.get("type").and_then(Value::as_str) == Some("PSEUDO")
}

fn is_measure_name(field: &Value) -> bool {
    field.get("type").and_then(Value::as_str) == Some("PSEUDO")
        && matches!(
            field.get("title").and_then(Value::as_str),
            Some(MEASURE_NAMES) | Some("Column Names")
        )
}

fn is_measure_value(field: &Value) -> bool {
    field.get("type").and_then(Value::as_str) == Some("PSEUDO")
        && field.get("title").and_then(Value::as_str) == Some("Measure Values")
}

fn is_number_field(field: &Value) -> bool {
    matches!(field_data_type(field), "integer" | "float")
}

fn is_date_field(field: &Value) -> bool {
    matches!(
        field_data_type(field),
        "date" | "datetime" | "genericdatetime"
    )
}

fn is_markup_field(field: &Value) -> bool {
    field_data_type(field) == "markup"
}

fn is_tree_data_type(data_type: &str) -> bool {
    matches!(data_type, "tree_str" | "tree_int" | "tree_float")
}

/// A conservative `isMarkupItem`: markup values are `{type, content/children}`
/// objects, everything else in pivot headers is plain.
fn is_markup_item(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key("type") && map.contains_key("content"),
        _ => false,
    }
}

fn fake_title_or_title(field: &Value) -> String {
    field
        .get("fakeTitle")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .or_else(|| field.get("title").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().map(|number| number != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `JSON.stringify` for a JS number: integral doubles stay integers, matching
/// the JavaScript output byte for byte.
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

fn js_number_to_string(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else {
        format!("{value}")
    }
}

/// `String(value)` for the scalar values that appear in pivot cells.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                integer.to_string()
            } else if let Some(unsigned) = number.as_u64() {
                unsigned.to_string()
            } else {
                js_number_to_string(number.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
        }
    }
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.is_empty() => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn get_param(params: &Value, key: &str) -> Option<Value> {
    match params.get(key)? {
        Value::Array(items) => items.first().cloned(),
        other => Some(other.clone()),
    }
}

fn get_current_page(params: &Value) -> f64 {
    match get_param(params, "_page") {
        Some(value) => number_value(&value).unwrap_or(f64::NAN),
        None => 1.0,
    }
}

// ---------------------------------------------------------------------------
// Formatting (`chartKitFormatNumberWrapper`, `formatDate`)
// ---------------------------------------------------------------------------

fn get_format_options(field: &Value) -> Option<Value> {
    if !is_number_field(field) {
        return None;
    }
    let mut options = field.get("formatting").cloned().unwrap_or_else(|| json!({}));
    let empty = options
        .as_object()
        .map(|map| map.is_empty())
        .unwrap_or(true);
    if empty {
        options = json!({});
    }
    if options.get("precision").and_then(Value::as_f64).is_none()
        && field_data_type(field) == "float"
    {
        if let Some(map) = options.as_object_mut() {
            map.insert("precision".to_string(), json!(2));
        }
    }
    Some(options)
}

fn group_thousands(text: &str) -> String {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, fraction) = match digits.split_once('.') {
        Some(parts) => parts,
        None => (digits, ""),
    };
    // Counter-based grouping instead of `(len - index) % 3` per character.
    let mut first_group = integer.len();
    while first_group > 3 {
        first_group -= 3;
    }
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3);
    let mut left = first_group;
    for character in integer.chars() {
        if left == 0 {
            grouped.push('\u{00a0}');
            left = 3;
        }
        grouped.push(character);
        left -= 1;
    }
    if fraction.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped},{fraction}")
    }
}

/// `shared/modules/format-units` `formatNumber` for the option subset tables use.
fn format_number(value: f64, options: &Value) -> Result<String, String> {
    if !value.is_finite() {
        return Ok("NaN".to_string());
    }
    let format = options
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("number");
    let mut multiplier = options
        .get("multiplier")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let prefix = options.get("prefix").and_then(Value::as_str).unwrap_or("");
    let postfix = options.get("postfix").and_then(Value::as_str).unwrap_or("");
    let label_mode = options.get("labelMode").and_then(Value::as_str);
    let mut percent_suffix = "";
    match format {
        "number" => {}
        "percent" => {
            multiplier = 100.0;
            percent_suffix = "%";
        }
        other => return Err(format!("number format `{other}` is not ported")),
    }
    if label_mode == Some("percent") {
        percent_suffix = "%";
    }
    let precision = options.get("precision").and_then(Value::as_f64);
    let show_rank_delimiter = options
        .get("showRankDelimiter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let unit = options.get("unit").and_then(Value::as_str);
    let unit_rate = options
        .get("unitRate")
        .and_then(Value::as_f64)
        .or(match unit {
            None | Some("default") => Some(0.0),
            Some("auto") => None,
            Some("k") => Some(1.0),
            Some("m") => Some(2.0),
            Some("b") => Some(3.0),
            Some("t") => Some(4.0),
            Some(_) => Some(0.0),
        });
    let scaled = value * multiplier;
    let rate = match unit_rate {
        Some(rate) => rate.clamp(0.0, 4.0),
        None => {
            let mut rate = 1u32;
            while rate < 4 && (scaled / 1000f64.powi(rate as i32)).abs() >= 1.0 {
                rate += 1;
            }
            (rate - 1) as f64
        }
    };
    let divided = scaled / 1000f64.powf(rate);
    let trimmed = match precision {
        Some(precision) if precision >= 0.0 => {
            let factor = 10f64.powf(precision);
            let rounded = (divided * factor).round() / factor;
            format!("{rounded:.precision$}", precision = precision as usize)
        }
        _ => js_number_to_string(divided),
    };
    let formatted = if show_rank_delimiter {
        group_thousands(&trimmed)
    } else {
        trimmed
    };
    let unit_text = TABLE_UNITS.get(rate as usize).copied().unwrap_or("");
    let delimiter = if unit_text.is_empty() { "" } else { " " };
    Ok(format!(
        "{prefix}{formatted}{delimiter}{unit_text}{percent_suffix}{postfix}"
    ))
}

fn chart_kit_format_number(value: f64, field: &Value) -> Result<String, String> {
    let mut options = get_format_options(field).unwrap_or_else(|| json!({}));
    if let Some(map) = options.as_object_mut() {
        map.insert("lang".to_string(), json!("ru"));
    }
    format_number(value, &options)
}

fn parse_date_value(value: &str) -> Result<chrono::NaiveDateTime, String> {
    use chrono::{NaiveDate, NaiveDateTime};
    if let Ok(datetime) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Ok(datetime);
    }
    if let Ok(datetime) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Ok(datetime);
    }
    if let Ok(datetime) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Ok(datetime);
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).ok_or_else(|| "midnight".to_string());
    }
    Err(format!("date value `{value}` is not parsed"))
}

fn format_date_value(
    value_type: &str,
    value: &Value,
    format: Option<&str>,
) -> Result<String, String> {
    
    let text = js_string(value);
    let datetime = parse_date_value(&text)?;
    let mask = format
        .map(|mask| mask.replace("hh", "HH"))
        .unwrap_or_else(|| match value_type {
            "datetime" | "genericdatetime" => DEFAULT_DATETIME_FORMAT.to_string(),
            _ => DEFAULT_DATE_FORMAT.to_string(),
        });
    render_moment_mask(datetime, &mask)
}

/// A small `moment().format()` subset used by pivot field formats (`YYYY-MM`, `DD.MM.YYYY`,
/// `MMMM YYYY`, ...). Unknown alphabetic tokens are rejected so unsupported masks keep the
/// "not ported" behaviour instead of silently rendering something else.
fn render_moment_mask(datetime: chrono::NaiveDateTime, mask: &str) -> Result<String, String> {
    use chrono::{Datelike, Timelike};
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August",
        "September", "October", "November", "December",
    ];
    const WEEKDAYS: [&str; 7] = [
        "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
    ];
    let characters: Vec<char> = mask.chars().collect();
    let mut out = String::with_capacity(mask.len());
    let mut index = 0usize;
    while index < characters.len() {
        let character = characters[index];
        if character == '[' {
            // moment escape: `[text]` is a literal
            let mut literal = String::new();
            index += 1;
            while index < characters.len() && characters[index] != ']' {
                literal.push(characters[index]);
                index += 1;
            }
            index += 1;
            out.push_str(&literal);
            continue;
        }
        if !character.is_ascii_alphabetic() {
            out.push(character);
            index += 1;
            continue;
        }
        let mut length = 0usize;
        while index + length < characters.len() && characters[index + length] == character {
            length += 1;
        }
        let token: String = characters[index..index + length].iter().collect();
        let rendered = match token.as_str() {
            "YYYY" => format!("{:04}", datetime.year()),
            "YY" => format!("{:02}", datetime.year().rem_euclid(100)),
            "MMMM" => MONTHS[(datetime.month0() as usize).min(11)].to_string(),
            "MMM" => MONTHS[(datetime.month0() as usize).min(11)][..3].to_string(),
            "MM" => format!("{:02}", datetime.month()),
            "M" => datetime.month().to_string(),
            "DD" => format!("{:02}", datetime.day()),
            "D" => datetime.day().to_string(),
            "HH" => format!("{:02}", datetime.hour()),
            "H" => datetime.hour().to_string(),
            "mm" => format!("{:02}", datetime.minute()),
            "m" => datetime.minute().to_string(),
            "ss" => format!("{:02}", datetime.second()),
            "s" => datetime.second().to_string(),
            "SSS" => format!("{:03}", datetime.and_utc().timestamp_subsec_millis()),
            "dddd" => {
                WEEKDAYS[datetime.weekday().num_days_from_sunday() as usize].to_string()
            }
            "ddd" => {
                WEEKDAYS[datetime.weekday().num_days_from_sunday() as usize][..3].to_string()
            }
            "Z" => "+00:00".to_string(),
            other => return Err(format!("date format `{other}` is not ported")),
        };
        out.push_str(&rendered);
        index += length;
    }
    Ok(out)
}

fn table_totals_styles() -> Value {
    json!({
        "background-color": TABLE_TOTALS_BG,
        "font-weight": 500,
    })
}

// ---------------------------------------------------------------------------
// Colors (`utils/color-helpers`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Rgb {
    red: i64,
    green: i64,
    blue: i64,
}

fn hex_to_rgb(hex: &str) -> Rgb {
    let channel = |range: std::ops::Range<usize>| -> i64 {
        hex.get(range)
            .and_then(|slice| i64::from_str_radix(slice, 16).ok())
            .unwrap_or(0)
    };
    Rgb {
        red: channel(1..3),
        green: channel(3..5),
        blue: channel(5..7),
    }
}

/// `getThresholdValues` (min, range, rangeMiddle, mid).
fn thresholds(color_values: &[f64], colors_config: &Value) -> (f64, f64, f64, f64) {
    let manual = colors_config.get("thresholdsMode").and_then(Value::as_str) == Some("manual");
    let list: Vec<f64> = color_values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    let auto_max = list.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let auto_min = list.iter().copied().fold(f64::INFINITY, f64::min);
    let max = if manual {
        colors_config
            .get("rightThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(auto_max)
    } else {
        auto_max
    };
    let min = if manual {
        colors_config
            .get("leftThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(auto_min)
    } else {
        auto_min
    };
    let range = max - min;
    let three_point =
        colors_config.get("gradientMode").and_then(Value::as_str) == Some("3-point");
    let mid = if manual
        && three_point
        && colors_config
            .get("middleThreshold")
            .and_then(Value::as_f64)
            .is_some()
    {
        colors_config
            .get("middleThreshold")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    } else {
        max - range / 2.0
    };
    let range_middle = if mid == range / 2.0 { mid } else { mid - min };
    (min, range, range_middle, mid)
}

fn rgb_color_value(
    delta: f64,
    gradient_mode: &str,
    range_middle_ratio: f64,
    colors: &[Rgb],
) -> String {
    let mut result_delta = delta;
    let (shade_a, shade_b) = if gradient_mode == "3-point" {
        if delta >= range_middle_ratio {
            result_delta = if delta == 1.0 {
                delta
            } else {
                delta - range_middle_ratio
            };
            (colors[1], colors[2])
        } else {
            (colors[0], colors[1])
        }
    } else {
        (colors[0], colors[1])
    };
    let red = ((shade_b.red - shade_a.red) as f64 * result_delta + shade_a.red as f64).floor() as i64;
    let green =
        ((shade_b.green - shade_a.green) as f64 * result_delta + shade_a.green as f64).floor() as i64;
    let blue =
        ((shade_b.blue - shade_a.blue) as f64 * result_delta + shade_a.blue as f64).floor() as i64;
    format!("rgb({red}, {green}, {blue})")
}

// ---------------------------------------------------------------------------
// Per-field settings (`settingsByField`)
// ---------------------------------------------------------------------------

/// `selectServerPalette` (`server/constants/index.ts:47`), system palettes only.
///
/// shortcut: the tenant default palette is `default20` (the open-source default the engine
/// starts with); a deployment that overrides `DEFAULT_COLOR_PALETTE_ID` would resolve
/// palette-index bar colours against a different palette.
fn select_server_palette(palette: &str, default_palette_id: &str) -> Vec<String> {
    let palettes = crate::graph_engine::system_palettes();
    let scheme = |id: &str| -> Option<Vec<String>> {
        if id.is_empty() {
            return None;
        }
        palettes
            .get(id)
            .and_then(|entry| entry.get("scheme"))
            .and_then(Value::as_array)
            .map(|scheme| scheme.iter().filter_map(Value::as_str).map(str::to_string).collect())
    };
    scheme(palette)
        .or_else(|| scheme(default_palette_id))
        .unwrap_or_default()
}

fn palette_color(colors: &[String], color_index: Option<i64>, color: Option<&str>, fallback: usize) -> Option<String> {
    if let Some(index) = color_index {
        return colors
            .get(index.max(0) as usize)
            .or_else(|| colors.get(fallback))
            .cloned();
    }
    if let Some(color) = color.filter(|color| !color.is_empty()) {
        return Some(color.to_string());
    }
    colors.get(fallback).cloned()
}

/// `getMinAndMaxValues` for the bar scale (`helpers/barsSettings/index.ts:26-35`).
fn bar_min_max(column_values: &[Value]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in column_values {
        let Some(number) = number_value(value) else {
            continue;
        };
        min = min.min(number);
        max = max.max(number);
    }
    (min, max)
}

/// `getBarSettingsViewOptions` (`helpers/barsSettings/index.ts:113-158`).
fn bar_view_options(bars: &Value, column_values: &[Value]) -> Map<String, Value> {
    let mut options = Map::new();
    options.insert("view".to_string(), json!("bar"));
    options.insert(
        "showLabel".to_string(),
        json!(bars
            .get("showLabels")
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    let scale = bars.get("scale").cloned().unwrap_or_else(|| json!({}));
    let (min, max) = if scale.get("mode").and_then(Value::as_str) == Some("auto") {
        bar_min_max(column_values)
    } else {
        let settings = scale.get("settings").cloned().unwrap_or_else(|| json!({}));
        // `Number(undefined)` is `NaN`, `Number(null)` is `0`.
        let number = |value: Option<&Value>| match value {
            None => f64::NAN,
            Some(Value::Null) => 0.0,
            Some(other) => number_value(other).unwrap_or(f64::NAN),
        };
        (
            number(settings.get("min")),
            number(settings.get("max")),
        )
    };
    let is_min_correct = !min.is_nan() && min <= 0.0;
    let is_max_correct = !max.is_nan() && max >= 0.0;
    if is_min_correct && is_max_correct && min == max {
        options.insert("max".to_string(), json_number(min));
    } else {
        if is_min_correct {
            options.insert("min".to_string(), json_number(min));
        }
        if is_max_correct {
            options.insert("max".to_string(), json_number(max));
        }
    }
    if let Some(align) = bars.get("align").and_then(Value::as_str) {
        if align != "default" {
            options.insert("align".to_string(), json!(align));
        }
    }
    options
}

/// `getGradientBarColor` (`helpers/barsSettings/index.ts:82-110`).
fn bar_gradient_color(settings: &Value, column_values: &[Value], row_value: &Value) -> String {
    let gradient_type = settings
        .get("gradientType")
        .and_then(Value::as_str)
        .unwrap_or("2-point");
    let gradient_id = settings
        .get("palette")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reversed = settings
        .get("reversed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut colors: Vec<Rgb> = gradient_colors(gradient_type, gradient_id)
        .iter()
        .map(|hex| hex_to_rgb(hex))
        .collect();
    if reversed {
        colors.reverse();
    }
    let thresholds = settings
        .get("thresholds")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let (auto_min, auto_max) = bar_min_max(column_values);
    let manual = thresholds.get("mode").and_then(Value::as_str) == Some("manual");
    let number = |value: Option<&Value>, fallback: f64| match value {
        Some(Value::Null) => 0.0,
        Some(other) => number_value(other).unwrap_or(fallback),
        None => fallback,
    };
    let max = if manual && thresholds.get("max").is_some() {
        number(thresholds.get("max"), auto_max)
    } else {
        auto_max
    };
    let min = if manual && thresholds.get("min").is_some() {
        number(thresholds.get("min"), auto_min)
    } else {
        auto_min
    };
    let range = max - min;
    let mid = if manual && thresholds.get("mid").is_some() {
        number(thresholds.get("mid"), range / 2.0)
    } else {
        range / 2.0
    };
    let range_middle = if mid == range / 2.0 { mid } else { mid - min };
    let range_middle_ratio = if range == 0.0 { 0.0 } else { range_middle / range };
    let delta = if range == 0.0 {
        0.5
    } else {
        match number_value(row_value) {
            Some(value) => ((value - min) / range).clamp(0.0, 1.0),
            None => return String::new(),
        }
    };
    if colors.len() < 2 {
        return String::new();
    }
    rgb_color_value(delta, gradient_type, range_middle_ratio, &colors)
}

/// `getBarSettingsValue` (`helpers/barsSettings/index.ts:159-231`).
fn bar_cell_settings(
    field: &Value,
    row_value: &Value,
    bars: &PivotBars,
    is_total_cell: bool,
) -> (Value, String, Option<String>, bool) {
    let bars_settings = field
        .get("barsSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let color_settings = bars_settings
        .pointer("/colorSettings/settings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let color_type = bars_settings
        .pointer("/colorSettings/colorType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let palette = color_settings
        .get("palette")
        .and_then(Value::as_str)
        .unwrap_or("");
    let palette_colors = select_server_palette(palette, "default20");
    let color_index = |key: &str| color_settings.get(key).and_then(Value::as_i64);
    let color = |key: &str| color_settings.get(key).and_then(Value::as_str);
    let bar_color = match color_type {
        "one-color" => palette_color(&palette_colors, color_index("colorIndex"), color("color"), 0),
        "two-color" => {
            let positive = number_value(row_value).map(|number| number >= 0.0).unwrap_or(true);
            if positive {
                palette_color(&palette_colors, color_index("positiveColorIndex"), color("positiveColor"), 2)
            } else {
                palette_color(&palette_colors, color_index("negativeColorIndex"), color("negativeColor"), 1)
            }
        }
        "gradient" => Some(bar_gradient_color(&color_settings, &bars.column_values, row_value))
            .filter(|color| !color.is_empty()),
        _ => None,
    };
    let format_options = get_format_options(field).unwrap_or_else(|| json!({}));
    let (value, formatted_value) = match number_value(row_value) {
        Some(number) => (json_number(number), format_number(number, &format_options).unwrap_or_default()),
        None => (json_number(0.0), js_string(row_value)),
    };
    let show_in_totals = bars_settings
        .get("showBarsInTotals")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let show_bar = !is_total_cell || show_in_totals;
    (value, formatted_value, bar_color, show_bar)
}

/// Bars settings for one field (`settingsByField[guid].barsSettings`).
struct PivotBars {
    options: Map<String, Value>,
    column_values: Vec<Value>,
}

/// `TableFieldBackgroundSettings` of one field (`settingsByField[guid].backgroundSettings`).
struct FieldBackground {
    settings: Value,
    color_field_guid: String,
    is_continuous: bool,
}

fn field_backgrounds(ctx: &PivotContext<'_>) -> HashMap<String, FieldBackground> {
    let mut backgrounds = HashMap::new();
    for (key, item) in &ctx.field_dict {
        let Some(settings) = item.get("backgroundSettings").filter(|settings| {
            settings
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        }) else {
            continue;
        };
        backgrounds.insert(
            key.clone(),
            FieldBackground {
                color_field_guid: settings
                    .get("colorFieldGuid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                is_continuous: settings
                    .pointer("/settings/isContinuous")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                settings: settings.clone(),
            },
        );
    }
    backgrounds
}

/// `getDiscreteColorValue` (`helpers/backgroundColor.ts:31-63`).
fn discrete_background_color(background: &FieldBackground, color_value: &Value) -> Option<String> {
    let palette_state = background
        .settings
        .pointer("/settings/paletteState")
        .filter(|state| state.is_object())?;
    if !js_truthy(color_value) {
        return None;
    }
    let mounted = palette_state.get("mountedColors")?.as_object()?;
    let index = number_value(mounted.get(&js_string(color_value))?)?;
    // `if (!colorIndex) return null`: index 0 is falsy in JS.
    if index == 0.0 || !index.is_finite() {
        return None;
    }
    let colors = select_server_palette(
        palette_state.get("palette").and_then(Value::as_str).unwrap_or(""),
        "default20",
    );
    if colors.is_empty() {
        return None;
    }
    let position = (index as i64).rem_euclid(colors.len() as i64) as usize;
    colors.get(position).cloned()
}

/// `colorizePivotTableByFieldBackgroundSettings` (`helpers/backgroundColor.ts:294-391`).
fn colorize_rows_by_field_background(
    ctx: &PivotContext<'_>,
    rows: &mut [Value],
    rows_data: &[Value],
    annotations: &HashMap<i64, String>,
) -> Result<(), String> {
    let backgrounds = field_backgrounds(ctx);
    if backgrounds.is_empty() || rows_data.is_empty() {
        return Ok(());
    }
    // `prepareBackgroundColorSettings`: collect the annotated values per colour field.
    let mut continuous_values: HashMap<String, HashSet<String>> = HashMap::new();
    let mut discrete_colors: HashMap<String, HashMap<String, Option<String>>> = HashMap::new();
    for row in rows_data {
        let Some(values) = row.get("values").and_then(Value::as_array) else {
            continue;
        };
        for cell in values {
            if !js_truthy(cell) {
                continue;
            }
            let Some(dataset_field) = dataset_field_from_cell(ctx, cell) else {
                continue;
            };
            let Some(guid) = field_id(&dataset_field) else {
                continue;
            };
            let Some(background) = backgrounds.get(guid) else {
                continue;
            };
            let Some(annotation) = get_annotation(cell, annotations, "background-color") else {
                continue;
            };
            let color_value = annotation.get(0).cloned().unwrap_or(Value::Null);
            if background.is_continuous {
                continuous_values
                    .entry(background.color_field_guid.clone())
                    .or_default()
                    .insert(js_string(&color_value));
                continue;
            }
            discrete_colors
                .entry(guid.to_string())
                .or_default()
                .insert(
                    js_string(&color_value),
                    discrete_background_color(background, &color_value),
                );
        }
    }
    let mut continuous: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (color_guid, values) in &continuous_values {
        let Some(background) = backgrounds
            .values()
            .find(|background| background.is_continuous && &background.color_field_guid == color_guid)
        else {
            continue;
        };
        let gradient_state = background
            .settings
            .pointer("/settings/gradientState")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let null_as_zero =
            gradient_state.get("nullMode").and_then(Value::as_str) == Some("as-0");
        let numeric = |value: &str| -> Option<f64> {
            if value == "null" {
                if null_as_zero {
                    Some(0.0)
                } else {
                    None
                }
            } else {
                number_value(&Value::String(value.to_string()))
            }
        };
        let numbers: Vec<f64> = values.iter().filter_map(|value| numeric(value)).collect();
        if numbers.is_empty() {
            continue;
        }
        let min = numbers.iter().copied().fold(f64::INFINITY, f64::min);
        let max = numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut map = HashMap::new();
        for value in values {
            let Some(number) = numeric(value) else {
                continue;
            };
            if let Some(color) = gradient_background(number, &gradient_state, (min, max)) {
                map.insert(value.clone(), color);
            }
        }
        continuous.insert(color_guid.clone(), map);
    }

    let row_header_length = rows_data
        .first()
        .and_then(|row| row.pointer("/header_with_info/cells"))
        .and_then(Value::as_array)
        .map(|cells| cells.len())
        .unwrap_or(0);
    for (row_index, row) in rows.iter_mut().enumerate() {
        let Some(cells) = row.get_mut("cells").and_then(Value::as_array_mut) else {
            continue;
        };
        if row_index >= rows_data.len() {
            break;
        }
        for index in row_header_length..cells.len() {
            let cell = &mut cells[index];
            if cell.get("id").and_then(Value::as_str).is_none()
                || cell.get("isTotalCell").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let cell_id = cell.get("id").and_then(Value::as_str).unwrap_or("");
            let guid = cell_id
                .strip_prefix("fieldId=")
                .unwrap_or(cell_id)
                .split("__")
                .next()
                .unwrap_or("");
            let Some(background) = backgrounds.get(guid) else {
                continue;
            };
            let color_key = cell.get("colorKey").cloned().unwrap_or(Value::Null);
            let color = if background.is_continuous {
                continuous
                    .get(&background.color_field_guid)
                    .and_then(|map| map.get(&js_string(&color_key)))
                    .cloned()
            } else {
                let mut color = discrete_colors
                    .get(guid)
                    .and_then(|map| map.get(&js_string(&color_key)))
                    .cloned()
                    .flatten();
                // `TODO: CHARTS-7124` branch: the backend cannot annotate `Measure Names`,
                // so the colour is resolved from the measure name on our side.
                if color.is_none() && background.color_field_guid == MEASURE_NAMES {
                    color = ctx
                        .field_dict
                        .get(guid)
                        .map(fake_title_or_title)
                        .and_then(|name| {
                            discrete_background_color(background, &json!(name))
                        });
                }
                color
            };
            let Some(color) = color else {
                continue;
            };
            let mut css = cell
                .get("css")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            css.insert("color".to_string(), json!("#FFF"));
            css.insert("backgroundColor".to_string(), json!(color));
            cell["css"] = Value::Object(css);
        }
    }
    Ok(())
}

/// `getValuesByField` (`helpers/misc.ts:46-91`): every value of a measure in the pivot rows,
/// including its sub-total/grand-total values when `showBarsInTotals` is on.
fn pivot_values_by_field(
    ctx: &PivotContext<'_>,
    pivot_data_rows: &[Value],
) -> HashMap<String, Vec<Value>> {
    let mut values: HashMap<String, Vec<Value>> = HashMap::new();
    for row in pivot_data_rows {
        let Some(row_values) = row.get("values").and_then(Value::as_array) else {
            continue;
        };
        for row_value in row_values {
            if !js_truthy(row_value) {
                continue;
            }
            let Some((value, pivot_field, _)) = cell_identity(ctx, row_value) else {
                continue;
            };
            let Some(guid) = field_id(&pivot_field).map(str::to_string) else {
                continue;
            };
            let is_total = pivot_field.pointer("/role_spec/role").and_then(Value::as_str)
                == Some("total");
            if is_total {
                let show_in_totals = ctx
                    .field_dict
                    .get(&guid)
                    .filter(|item| {
                        item.pointer("/barsSettings/enabled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .and_then(|item| item.pointer("/barsSettings/showBarsInTotals"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !show_in_totals {
                    continue;
                }
            }
            values.entry(guid).or_default().push(value);
        }
    }
    values
}

fn pivot_bars_by_field(
    settings: &HashMap<String, Value>,
    values_by_field: &HashMap<String, Vec<Value>>,
) -> HashMap<String, PivotBars> {
    let mut bars: HashMap<String, PivotBars> = HashMap::new();
    for (key, item) in settings {
        if !item
            .pointer("/barsSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(guid) = field_id(item) else {
            continue;
        };
        let column_values = values_by_field.get(guid).cloned().unwrap_or_default();
        let bars_settings = item
            .get("barsSettings")
            .cloned()
            .unwrap_or_else(|| json!({}));
        bars.insert(
            key.clone(),
            PivotBars {
                options: bar_view_options(&bars_settings, &column_values),
                column_values,
            },
        );
    }
    bars
}


/// `colorizePivotTableCell` background for one value, or `None`.
fn gradient_background(value: f64, colors_config: &Value, min_max: (f64, f64)) -> Option<String> {
    let gradient_mode = colors_config
        .get("gradientMode")
        .and_then(Value::as_str)
        .unwrap_or("2-point");
    let (min, range, range_middle, _mid) = thresholds(&[min_max.0, min_max.1], colors_config);
    let delta = if range == 0.0 {
        0.5
    } else {
        ((value - min) / range).clamp(0.0, 1.0)
    };
    let range_middle_ratio = if range == 0.0 { 0.0 } else { range_middle / range };
    let gradient_id = colors_config
        .get("gradientPalette")
        .and_then(Value::as_str)
        .unwrap_or("");
    let reversed = colors_config
        .get("reversed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut colors: Vec<Rgb> = gradient_colors(gradient_mode, gradient_id)
        .iter()
        .map(|hex| hex_to_rgb(hex))
        .collect();
    if reversed {
        colors.reverse();
    }
    if colors.len() < 2 {
        return None;
    }
    Some(rgb_color_value(delta, gradient_mode, range_middle_ratio, &colors))
}

// ---------------------------------------------------------------------------
// Public guard
// ---------------------------------------------------------------------------

/// Reject charts whose JS behaviour is not ported, so the caller can fall back.
pub fn supports(shared: &Value) -> Result<(), String> {
    let visualization = shared
        .get("visualization")
        .ok_or_else(|| "no visualization".to_string())?;
    if visualization.get("id").and_then(Value::as_str) != Some("pivotTable") {
        return Err("not a pivot table".to_string());
    }
    let extra = shared
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if extra.get("pivotFallback").and_then(Value::as_str) == Some("on") {
        return Err("pivotFallback is on".to_string());
    }
    // Layers are a graph-visualization feature; the pivot preparer never reads them, so an
    // empty/absent list is fine and a non-empty one is a config the pivot builder cannot use.
    if visualization
        .get("layers")
        .and_then(Value::as_array)
        .map(|layers| !layers.is_empty())
        .unwrap_or(false)
    {
        return Err("layered pivot tables are not ported".to_string());
    }
    let datasets = shared
        .get("datasetsIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if datasets.len() != 1 {
        return Err("only a single-dataset pivot is ported".to_string());
    }
    // `links` only matter for filters of a *second* dataset, which the single-dataset guard
    // above already rejects; the reference pivot ignores them for one dataset.
    for key in ["segments", "shapes", "labels", "tooltips"] {
        if shared
            .get(key)
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false)
        {
            return Err(format!("pivot with `{key}` is not ported"));
        }
    }
    let placeholders = visualization
        .get("placeholders")
        .and_then(Value::as_array)
        .ok_or_else(|| "no placeholders".to_string())?;
    for id in ["pivot-table-columns", "rows", "measures"] {
        let placeholder = placeholders
            .iter()
            .find(|placeholder| placeholder.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("pivot placeholder `{id}` is missing"))?;
        let items = placeholder
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in &items {
            if field_id(item).is_none() && !is_measure_name(item) && !is_measure_value(item) {
                return Err("placeholder item without a guid".to_string());
            }
            if is_parameter(item) {
                return Err("parameters in pivot placeholders are not ported".to_string());
            }
            // Measure Names / Measure Values are pseudo fields the backend resolves itself
            // (`ref: {type: "measure_name"}`); any other pseudo field is not ported.
            if is_pseudo_field(item) && !is_measure_name(item) && !is_measure_value(item) {
                return Err("pseudo fields in pivot placeholders are not ported".to_string());
            }
            if is_tree_data_type(field_data_type(item)) {
                return Err("tree fields in pivot placeholders are not ported".to_string());
            }
        }
    }
    let measures = placeholders
        .iter()
        .find(|placeholder| placeholder.get("id").and_then(Value::as_str) == Some("measures"))
        .and_then(|placeholder| placeholder.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if measures.iter().any(is_measure_name) {
        return Err("Measure Names in the measures placeholder is not ported".to_string());
    }
    if extra.get("pagination").and_then(Value::as_str) == Some("on") {
        match extra.get("limit") {
            Some(limit) if number_value(limit).is_some() => {}
            _ => return Err("pivot pagination without a numeric limit is not ported".to_string()),
        }
    }
    if visualization
        .pointer("/id")
        .and_then(Value::as_str)
        == Some("pivotTable")
    {
        for placeholder in placeholders {
            if let Some(items) = placeholder.get("items").and_then(Value::as_array) {
                for item in items {
                    if field_data_type(item) == "markup" {
                        return Err("markup pivot fields are not ported".to_string());
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Params resolution (`expose.js` `resolveParams`)
// ---------------------------------------------------------------------------

/// Resolve `__relative*` / `__interval_*` parameter values.
///
/// Only interval values whose endpoints are already absolute timestamps are
/// supported: the JS `resolveIntervalDate` returns them unchanged.
/// `__relative_[+-]<amount><scale>[_<s|e><unit>]` from `charts_shared`.
///
/// Returns the resolved UTC timestamp in `toISOString` form. The parser mirrors
/// `resolveDlRelative` (`shared/modules/helpers`): year/quarter/month/week durations are
/// calendar arithmetic, `CLIP_SCALES` snap to the start/end of the day and an explicit cast
/// (`_sd`, `_ey`, ...) snaps to the start/end of the given unit.
fn resolve_relative_date(value: &str, interval_part: &str) -> Option<String> {
    use chrono::{Datelike, Duration, Months, Timelike, Utc};

    let rest = value.strip_prefix("__relative_")?;
    let mut chars = rest.chars();
    let sign = match chars.next()? {
        '+' => 1i64,
        '-' => -1i64,
        _ => return None,
    };
    let mut amount_text = String::new();
    let mut scale = String::new();
    let mut cast: Option<(char, char)> = None;
    let mut iterator = rest[1..].chars().peekable();
    while let Some(character) = iterator.next() {
        if character.is_ascii_digit() {
            if !scale.is_empty() && scale != "ms" {
                // digits after the scale are only valid inside a cast suffix
                return None;
            }
            amount_text.push(character);
            continue;
        }
        if character == '_' {
            let cast_kind = iterator.next()?;
            let cast_scale = iterator.next()?;
            cast = Some((cast_kind, cast_scale));
            break;
        }
        if character == 'm' && iterator.peek() == Some(&'s') {
            iterator.next();
            scale = "ms".to_string();
            continue;
        }
        if matches!(character, 'y' | 'Q' | 'M' | 'w' | 'd' | 'h' | 'm' | 's') {
            scale.push(character);
            continue;
        }
        return None;
    }
    if amount_text.is_empty() || scale.is_empty() {
        return None;
    }
    let amount = amount_text.parse::<i64>().ok()? * sign;
    let now = Utc::now().naive_utc();
    let mut datetime = now;
    match scale.as_str() {
        "y" => datetime = datetime.checked_add_months(Months::new((amount * 12).unsigned_abs() as u32))?,
        "Q" => datetime = datetime.checked_add_months(Months::new((amount * 3).unsigned_abs() as u32))?,
        "M" => datetime = datetime.checked_add_months(Months::new(amount.unsigned_abs() as u32))?,
        "w" => datetime = datetime.checked_add_signed(Duration::days(7 * amount))?,
        "d" => datetime = datetime.checked_add_signed(Duration::days(amount))?,
        "h" => datetime = datetime.checked_add_signed(Duration::hours(amount))?,
        "m" => datetime = datetime.checked_add_signed(Duration::minutes(amount))?,
        "s" => datetime = datetime.checked_add_signed(Duration::seconds(amount))?,
        "ms" => datetime = datetime.checked_add_signed(Duration::milliseconds(amount))?,
        _ => return None,
    }
    // Negative calendar deltas cannot be expressed with `checked_add_months` (it only counts
    // forward), so apply the backwards case with `checked_sub_months`.
    if amount < 0 && matches!(scale.as_str(), "y" | "Q" | "M") {
        let months = match scale.as_str() {
            "y" => amount * 12,
            "Q" => amount * 3,
            _ => amount,
        };
        datetime = now
            .checked_sub_months(Months::new(months.unsigned_abs() as u32))
            .unwrap_or(datetime);
    }
    let end = interval_part == "end";
    let snap = |datetime: chrono::NaiveDateTime, unit: char, end: bool| -> Option<chrono::NaiveDateTime> {
        let (start, end_of_unit) = match unit {
            'y' => {
                let start = chrono::NaiveDate::from_ymd_opt(datetime.year(), 1, 1)?.and_hms_opt(0, 0, 0)?;
                let end = chrono::NaiveDate::from_ymd_opt(datetime.year(), 12, 31)?.and_hms_milli_opt(23, 59, 59, 999)?;
                (start, end)
            }
            'Q' => {
                let quarter_month = ((datetime.month0() / 3) * 3) + 1;
                let start = chrono::NaiveDate::from_ymd_opt(datetime.year(), quarter_month, 1)?.and_hms_opt(0, 0, 0)?;
                let last_month = quarter_month + 2;
                let (year, month) = if last_month <= 12 {
                    (datetime.year(), last_month)
                } else {
                    (datetime.year() + 1, last_month - 12)
                };
                let last_day = days_in_month(year, month)?;
                let end = chrono::NaiveDate::from_ymd_opt(year, month, last_day)?.and_hms_milli_opt(23, 59, 59, 999)?;
                (start, end)
            }
            'M' => {
                let start = chrono::NaiveDate::from_ymd_opt(datetime.year(), datetime.month(), 1)?.and_hms_opt(0, 0, 0)?;
                let last_day = days_in_month(datetime.year(), datetime.month())?;
                let end = chrono::NaiveDate::from_ymd_opt(datetime.year(), datetime.month(), last_day)?.and_hms_milli_opt(23, 59, 59, 999)?;
                (start, end)
            }
            'w' => {
                let days_from_monday = datetime.weekday().num_days_from_monday() as i64;
                let start_date = datetime.date() - Duration::days(days_from_monday);
                let start = start_date.and_hms_opt(0, 0, 0)?;
                let end = (start_date + Duration::days(6)).and_hms_milli_opt(23, 59, 59, 999)?;
                (start, end)
            }
            'd' => {
                let start = datetime.date().and_hms_opt(0, 0, 0)?;
                let end = datetime.date().and_hms_milli_opt(23, 59, 59, 999)?;
                (start, end)
            }
            'h' => {
                let start = datetime.date().and_hms_opt(datetime.hour(), 0, 0)?;
                let end = datetime.date().and_hms_milli_opt(datetime.hour(), 59, 59, 999)?;
                (start, end)
            }
            'm' => {
                let start = datetime.date().and_hms_opt(datetime.hour(), datetime.minute(), 0)?;
                let end = datetime.date().and_hms_milli_opt(datetime.hour(), datetime.minute(), 59, 999)?;
                (start, end)
            }
            's' => {
                let start = datetime.date().and_hms_opt(datetime.hour(), datetime.minute(), datetime.second())?;
                let end = datetime.date().and_hms_milli_opt(datetime.hour(), datetime.minute(), datetime.second(), 999)?;
                (start, end)
            }
            _ => return None,
        };
        Some(if end { end_of_unit } else { start })
    };
    if let Some((cast_kind, cast_scale)) = cast {
        if !matches!(cast_kind, 's' | 'e') {
            return None;
        }
        if !matches!(cast_scale, 'y' | 'Q' | 'M' | 'w' | 'd' | 'h' | 'm' | 's') {
            return None;
        }
        datetime = snap(datetime, cast_scale, cast_kind == 'e')?;
    } else if matches!(scale.as_str(), "d" | "w" | "M" | "Q" | "y") {
        // `CLIP_SCALES`: every clipped scale snaps to the start/end of the day.
        datetime = snap(datetime, 'd', end)?;
    }
    Some(format_iso(datetime))
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    use chrono::NaiveDate;
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    Some((next - first).num_days() as u32)
}

fn format_iso(datetime: chrono::NaiveDateTime) -> String {
    datetime.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn resolve_relative_param(value: &str, interval_part: &str) -> Result<String, String> {
    resolve_relative_date(value, interval_part)
        .ok_or_else(|| format!("relative date `{value}` is not resolved"))
}

/// Split `__interval_<from>_<to>` where either side may be a `__relative_*` marker.
fn split_interval(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("__interval_")?;
    if let Some(relative_rest) = rest.strip_prefix("__relative_") {
        let mut offset = "__relative_".len();
        let mut chars = relative_rest.chars().peekable();
        if !matches!(chars.peek(), Some('+') | Some('-')) {
            return None;
        }
        offset += 1;
        chars.next();
        let mut digits = 0usize;
        while matches!(chars.peek(), Some(character) if character.is_ascii_digit()) {
            chars.next();
            offset += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        let mut scale_chars = 0usize;
        if chars.peek() == Some(&'m') {
            chars.next();
            offset += 1;
            if chars.peek() == Some(&'s') {
                chars.next();
                offset += 1;
            }
            scale_chars += 1;
        } else if matches!(chars.peek(), Some('y' | 'Q' | 'M' | 'w' | 'd' | 'h' | 's')) {
            chars.next();
            offset += 1;
            scale_chars += 1;
        }
        if scale_chars == 0 {
            return None;
        }
        if chars.peek() == Some(&'_') {
            // A cast suffix is `_s<unit>` / `_e<unit>`; anything else (`__relative...`,
            // an absolute date) is the interval separator and belongs to the `to` token.
            let remaining: String = chars.clone().collect();
            let mut cast_chars = remaining.chars();
            let is_cast = cast_chars.next() == Some('_')
                && matches!(cast_chars.next(), Some('s') | Some('e'))
                && matches!(cast_chars.next(), Some('y' | 'Q' | 'M' | 'w' | 'd' | 'h' | 'm' | 's'));
            if is_cast {
                chars.next();
                offset += 1;
                let _cast_kind = chars.next()?;
                offset += 1;
                let _cast_scale = chars.next()?;
                offset += 1;
            }
        }
        let from = rest[..offset].to_string();
        let remainder = &rest[offset..];
        let to = remainder.strip_prefix('_')?;
        if to.is_empty() {
            return None;
        }
        return Some((from, to.to_string()));
    }
    let (from, to) = rest.split_once('_')?;
    if from.is_empty() || to.is_empty() {
        return None;
    }
    Some((from.to_string(), to.to_string()))
}

fn resolve_interval(value: &str) -> Option<(String, String)> {
    let (from, to) = split_interval(value)?;
    let start = if from.starts_with("__relative_") {
        resolve_relative_date(&from, "start")?
    } else {
        parse_date_value(&from).ok()?;
        from
    };
    let end = if to.starts_with("__relative_") {
        resolve_relative_date(&to, "end")?
    } else {
        parse_date_value(&to).ok()?;
        to
    };
    Some((start, end))
}

pub fn resolve_params(params: &Value) -> Result<Value, String> {
    let mut resolved = params.clone();
    let Some(map) = resolved.as_object_mut() else {
        return Ok(resolved);
    };
    for values in map.values_mut() {
        let Some(items) = values.as_array_mut() else {
            continue;
        };
        for value in items.iter_mut() {
            let Some(text) = value.as_str().map(str::to_string) else {
                continue;
            };
            if text.starts_with("__relative") {
                *value = Value::String(resolve_relative_param(&text, "start")?);
                continue;
            }
            if text.starts_with("__interval") {
                let Some((from, to)) = resolve_interval(&text) else {
                    return Err(format!("malformed __interval param `{text}`"));
                };
                *value = Value::String(format!("__interval_{from}_{to}"));
            }
        }
    }
    Ok(resolved)
}

// ---------------------------------------------------------------------------
// Request building (`url/build-request-body`)
// ---------------------------------------------------------------------------

fn raw_param_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => {
            !(items.len() == 1 && items.first().and_then(Value::as_str) == Some(""))
        }
        _ => true,
    }
}

fn param_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Number(number) => !number.as_f64().map(f64::is_nan).unwrap_or(false),
        Value::Array(items) => {
            !items.is_empty()
                && !items.iter().any(Value::is_null)
                && !(items.len() == 1 && items.first().and_then(Value::as_str) == Some(""))
        }
        _ => true,
    }
}

/// `resolveOperation(rawValue, defaultOperation)` + `prepareArrayFilterValues`.
fn prepare_filter_values(values: &[Value], field: Option<&Value>) -> (Vec<Value>, Vec<String>) {
    let mut result_values: Vec<Value> = Vec::new();
    let mut operations: Vec<String> = Vec::new();
    for raw in values {
        let default_operation = if field.map(is_date_field).unwrap_or(false) && values.len() == 1 {
            "EQ"
        } else {
            "IN"
        };
        let text = raw.as_str().unwrap_or("");
        let mut operation = default_operation.to_string();
        let mut value = raw.clone();
        if text.starts_with("__relative") {
            return (Vec::new(), Vec::new());
        } else if let Some(rest) = text.strip_prefix("__interval_") {
            if let Some((from, to)) = rest.split_once('_') {
                value = json!([from, to]);
                operation = "BETWEEN".to_string();
            }
        }
        if !value.is_null() {
            result_values.push(value);
            operations.push(operation);
        }
    }
    (result_values, operations)
}

/// Resolve the saved chart-filter values (`shared.filters`) the way the JS engine does
/// (`__dlHelpers.resolveParams` + `formatFilters`), returning the values and the operation.
///
/// Dashboard date-range filters are stored as a *single* string in the chart config:
///
/// ```json
/// {"filter": {"value": ["__interval___relative_-180d___relative_-2d"],
///             "operation": {"code": "BETWEEN"}}}
/// ```
///
/// The data-api `BETWEEN` filter requires exactly two values, so passing the raw marker
/// through produced `400 filter BETWEEN on <field> expects exactly two values, got 1` on
/// every chart that has a date-range filter saved in the chart itself (ClickHouse charts in
/// particular). Both interval sides may be `__relative_*` markers (`from` resolved as the
/// interval start, `to` as its end); a lone `__relative_*` value is resolved as one
/// timestamp (`start` for ascending operators, `end` for descending ones).
///
/// A value that looks like an interval but cannot be resolved is forwarded unchanged, exactly
/// like the JS `resolveIntervalDate` returning `null` does: the raw marker then reaches the
/// data-api as one value and it answers with its own `INVALID_VALUE` error. Silently dropping
/// it would change the filter instead of reporting it.
pub fn resolve_filter_values(values: &[Value], operation: &str) -> (Vec<Value>, String) {
    let descending = matches!(operation, "LT" | "LTE" | "LE");
    let interval_part = if descending { "end" } else { "start" };
    if values.len() == 1 {
        if let Some(text) = values[0].as_str() {
            if text.starts_with("__interval_") {
                return match resolve_interval(text) {
                    Some((from, to)) => (vec![json!(from), json!(to)], "BETWEEN".to_string()),
                    None => (values.to_vec(), operation.to_string()),
                };
            }
            if text.starts_with("__relative_") {
                return match resolve_relative_date(text, interval_part) {
                    Some(resolved) => (vec![json!(resolved)], operation.to_string()),
                    None => (values.to_vec(), operation.to_string()),
                };
            }
        }
    }
    let mut resolved = Vec::with_capacity(values.len());
    for value in values {
        if let Some(text) = value.as_str() {
            if text.starts_with("__interval_") {
                // An interval inside an `IN` list is the two interval bounds.
                if let Some((from, to)) = resolve_interval(text) {
                    resolved.push(json!(from));
                    resolved.push(json!(to));
                    continue;
                }
            } else if text.starts_with("__relative_") {
                if let Some(resolved_text) = resolve_relative_date(text, interval_part) {
                    resolved.push(json!(resolved_text));
                    continue;
                }
            }
        }
        resolved.push(value.clone());
    }
    (resolved, operation.to_string())
}

fn format_params_filters(
    params: &Value,
    schema: &[Value],
    dataset: &str,
) -> Vec<(String, String, Vec<Value>)> {
    let mut filters: Vec<(String, String, Vec<Value>)> = Vec::new();
    let Some(params_map) = params.as_object() else {
        return filters;
    };
    for (key, value) in params_map {
        if key == "__datasetId" || !raw_param_valid(value) || !param_valid(value) {
            continue;
        }
        let values: Vec<Value> = match value {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let found_field = schema.iter().find(|field| {
            field.get("guid").and_then(Value::as_str) == Some(key.as_str())
                || field.get("title").and_then(Value::as_str) == Some(key.as_str())
        });
        let mut guid = key.clone();
        if guid.len() != 36 {
            if let Some(field_guid) = found_field.and_then(field_id) {
                if field_guid != key {
                    guid = field_guid.to_string();
                }
            }
        }
        if !schema
            .iter()
            .any(|field| field_id(field) == Some(guid.as_str()))
        {
            // Linked-dataset params are not ported; a plain field guid from the
            // request (`scale` and friends) is kept as-is.
        }
        let _ = dataset;
        let (values_with_operations, operations) = prepare_filter_values(&values, found_field);
        filters.retain(|(column, _, _)| column != &guid);
        for operation in ["IN", "NIN"] {
            let grouped: Vec<Value> = values_with_operations
                .iter()
                .zip(operations.iter())
                .filter(|(_, op)| op.as_str() == operation)
                .map(|(value, _)| value.clone())
                .collect();
            if !grouped.is_empty() {
                filters.push((guid.clone(), operation.to_string(), grouped));
            }
        }
        for (value, operation) in values_with_operations.iter().zip(operations.iter()) {
            if operation != "IN" && operation != "NIN" {
                let values = match value {
                    Value::Array(items) => items.clone(),
                    other => vec![other.clone()],
                };
                filters.push((guid.clone(), operation.clone(), values));
            }
        }
    }
    filters
}

fn format_chart_filters(
    shared: &Value,
    dataset: &str,
    schema: &[Value],
) -> Result<Vec<(String, String, Vec<Value>)>, String> {
    let mut filters: Vec<(String, String, Vec<Value>)> = Vec::new();
    let Some(chart_filters) = shared.get("filters").and_then(Value::as_array) else {
        return Ok(filters);
    };
    for filter in chart_filters {
        if filter
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(filter_dataset) = filter.get("datasetId").and_then(Value::as_str) {
            if filter_dataset != dataset {
                continue;
            }
        }
        let Some(definition) = filter.get("filter") else {
            continue;
        };
        let operation = definition
            .pointer("/operation/code")
            .and_then(Value::as_str)
            .or_else(|| definition.get("operation").and_then(Value::as_str))
            .unwrap_or("IN")
            .to_string();
        let Some(value) = definition.get("value") else {
            continue;
        };
        let raw_values: Vec<Value> = match value {
            Value::Array(items) => items.clone(),
            Value::String(text) if text.is_empty() => continue,
            Value::Null => continue,
            other => vec![other.clone()],
        };
        if raw_values.is_empty() {
            continue;
        }
        // `__interval_*` / `__relative_*` markers from the saved chart config must be
        // resolved into the absolute values the data-api expects (see `resolve_filter_values`).
        let (values, operation) = resolve_filter_values(&raw_values, &operation);
        if values.is_empty() {
            continue;
        }
        let mut guid = filter
            .get("guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if guid.is_empty() {
            continue;
        }
        if guid.len() != 36 {
            if let Some(field_guid) = schema
                .iter()
                .find(|field| field.get("guid").and_then(Value::as_str) == Some(guid.as_str()))
                .and_then(field_id)
            {
                guid = field_guid.to_string();
            }
        }
        filters.push((guid, operation, values));
    }
    Ok(filters)
}

fn merge_filters(
    chart_filters: Vec<(String, String, Vec<Value>)>,
    params_filters: Vec<(String, String, Vec<Value>)>,
    params: &Value,
    schema: &[Value],
) -> Vec<Value> {
    let params_columns: HashSet<String> = params_filters
        .iter()
        .map(|(column, _, _)| column.clone())
        .collect();
    let mut merged: Vec<(String, String, Vec<Value>)> = chart_filters
        .into_iter()
        .filter(|(column, _, _)| !params_columns.contains(column))
        .collect();
    merged.extend(params_filters);
    merged
        .into_iter()
        .filter(|(column, operation, _)| {
            if operation == "NO_SELECTED_VALUES" {
                return false;
            }
            let Some(field) = schema
                .iter()
                .find(|field| field.get("guid").and_then(Value::as_str) == Some(column.as_str()))
            else {
                return true;
            };
            let field_guid = field_id(field).unwrap_or("");
            let field_title = field.get("title").and_then(Value::as_str).unwrap_or("");
            let param_filter = params.get(field_guid).or_else(|| params.get(field_title));
            match param_filter {
                None => true,
                Some(Value::Array(items)) => {
                    items.first().and_then(Value::as_str) != Some("")
                }
                Some(Value::String(text)) => !text.is_empty(),
                Some(Value::Null) => false,
                Some(_) => true,
            }
        })
        .map(|(column, operation, values)| {
            json!({"ref": {"type": "id", "id": column}, "operation": operation, "values": values})
        })
        .collect()
}

fn placeholder_items(shared: &Value, placeholder_id: &str) -> Vec<Value> {
    shared
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)
        .and_then(|placeholders| {
            placeholders.iter().find(|placeholder| {
                placeholder.get("id").and_then(Value::as_str) == Some(placeholder_id)
            })
        })
        .and_then(|placeholder| placeholder.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Field ids of a data-api request/result `fields` list in their wire order.
///
/// The list mixes `{ref: {type: "id", id}}` (result/request) and `{guid}` (schema); both are
/// accepted. This is the order the reference JS processor shows in `extra.datasets[*].
/// fieldsList` (used fields first), so it is carried into `build_extra` instead of guessing
/// from the visualization placeholders (filters/notes may use fields that are not there).
pub fn used_field_ids(fields: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(items) = fields.as_array() else {
        return out;
    };
    for item in items {
        let id = item
            .pointer("/ref/id")
            .and_then(Value::as_str)
            .or_else(|| item.get("guid").and_then(Value::as_str))
            .or_else(|| item.get("id").and_then(Value::as_str));
        if let Some(id) = id {
            if !out.iter().any(|known| known == id) {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn collect_placeholder_items<'a>(placeholders: Option<&'a Value>, items: &mut Vec<&'a Value>) {
    for placeholder in placeholders.and_then(Value::as_array).into_iter().flatten() {
        if let Some(placeholder_items) = placeholder.get("items").and_then(Value::as_array) {
            items.extend(placeholder_items.iter());
        }
    }
}

fn all_placeholder_items(shared: &Value) -> Vec<&Value> {
    // `getFieldList(..., visualization.placeholders)`: a combined chart carries its fields on
    // the layers, so the layer placeholders take part in the formatting lookup too.
    let mut items: Vec<&Value> = Vec::new();
    collect_placeholder_items(shared.pointer("/visualization/placeholders"), &mut items);
    // For a combined chart the reference `getFieldList` is called with the first layer's
    // placeholders (the layers are prepared one by one and the extra is assembled afterwards).
    if let Some(layer) = shared
        .pointer("/visualization/layers")
        .and_then(Value::as_array)
        .and_then(|layers| layers.first())
    {
        collect_placeholder_items(layer.get("placeholders"), &mut items);
    }
    items
}

fn merge_dataset_schema(dataset_fields: &Value, shared: &Value) -> Vec<Value> {
    let mut merged: Vec<Value> = dataset_fields.as_array().cloned().unwrap_or_default();
    if let Some(partials) = shared.get("datasetsPartialFields").and_then(Value::as_array) {
        for fields in partials {
            if let Some(items) = fields.as_array() {
                merged.extend(items.iter().cloned());
            }
        }
    }
    merged
}

/// A `flatTable` view of a pivot table: the rows/columns/measures placeholders flattened into
/// one `flat-table-columns` list (dimensions first, then measures), pseudo fields dropped.
///
/// The `pivotTable` shape is only partially ported (a single measure, no `Measure Names`
/// pseudo dimensions, no sub-totals). Instead of answering `501 chart type ... is not
/// supported` and leaving the dashboard widget empty, the pivot path falls back to this flat
/// table: every selected field becomes a column, every query row becomes a table row, so the
/// widget still shows its data. The fallback is explicit in the engine log
/// (`pivot: ...; flat-table fallback`) so the missing pivot features stay visible.
pub fn flat_table_shared(shared: &Value) -> Value {
    let mut flat = shared.clone();
    let mut items: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // `rows` first (the row dimensions read as the leading columns), then the column
    // dimensions of the pivot, then the measures.
    for placeholder_id in ["rows", "pivot-table-columns", "measures"] {
        for item in placeholder_items(shared, placeholder_id) {
            if is_measure_name(&item) || is_pseudo_field(&item) || is_parameter(&item) {
                continue;
            }
            let Some(id) = field_id(&item) else { continue };
            if !seen.insert(id.to_string()) {
                continue;
            }
            items.push(item);
        }
    }
    if let Some(object) = flat.as_object_mut() {
        object.insert(
            "visualization".to_string(),
            json!({
                "id": "flatTable",
                "placeholders": [{"id": "flat-table-columns", "items": items}],
            }),
        );
    }
    flat
}

/// `getPayloadWithCommonTableSettings`'s order_by for a back-end pivot table.
fn table_order_by(shared: &Value, params: &Value) -> Vec<(String, String)> {
    let is_pivot = shared.pointer("/visualization/id").and_then(Value::as_str) == Some("pivotTable");
    let items = all_placeholder_items(shared);
    let rows_items = placeholder_items(shared, "rows");
    let mut all_ids: HashSet<String> = items.iter().filter_map(|item| field_id(item)).map(str::to_string).collect();
    for item in shared
        .get("colors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if let Some(guid) = field_id(&item) {
            all_ids.insert(guid.to_string());
        }
    }

    // `getOrderByItemForColumnClickSort`
    let column_id = get_param(params, "_columnId")
        .map(|value| js_string(&value))
        .filter(|value| !value.is_empty());
    let order = get_param(params, "_sortOrder").and_then(|value| number_value(&value));
    let mut field_guid = None;
    if is_pivot {
        if let Some(column_id) = column_id.as_deref() {
            if let Some(rest) = column_id.split("_id=").nth(1) {
                if let Some(field_part) = rest.split("_name=").next() {
                    if let Some(guid_part) = field_part.split("__index").next() {
                        if let Some(guid) = guid_part.strip_prefix("fieldId=") {
                            field_guid = Some(guid.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut order_by: Vec<(String, String)> = Vec::new();
    if let (Some(guid), Some(order)) = (field_guid, order) {
        if order.is_finite() && order != 0.0 && all_ids.contains(&guid) {
            let allowed = if is_pivot {
                rows_items
                    .iter()
                    .filter_map(field_id)
                    .any(|row_guid| row_guid == guid.as_str())
            } else {
                true
            };
            if allowed {
                let direction = if order == 1.0 { "ASC" } else { "DESC" };
                order_by.push((guid, direction.to_string()));
            }
        }
    }
    for item in items.iter().filter(|item| is_dimension_field(item)) {
        if let Some(guid) = field_id(item) {
            if !order_by.iter().any(|(column, _)| column == guid) {
                order_by.push((guid.to_string(), "ASC".to_string()));
            }
        }
    }
    order_by
}

/// `getStructureWithSortingFromField` for one measure.
fn measure_sorting(
    params: &Value,
    measure_guid: &str,
    columns: &[Value],
    rows: &[Value],
    measures_len: usize,
) -> Result<Option<Value>, String> {
    let parse = |value: Option<Value>| -> Value {
        match value {
            Some(Value::String(text)) => {
                serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
            }
            Some(Value::Array(items)) => items
                .first()
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or_else(|| json!({})),
            _ => json!({}),
        }
    };
    let column = parse(get_param(params, "_sortColumnMeta"));
    let row = parse(get_param(params, "_sortRowMeta"));
    let fields_changed = |meta: &Value, fields: &[Value]| -> bool {
        let prev_order: Vec<String> = meta
            .get("fieldOrder")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(js_string).collect())
            .unwrap_or_default();
        let is_invisible_measure_name =
            prev_order.len() == 1 && fields.is_empty() && prev_order[0] == "measure_name";
        if prev_order.len() != fields.len() && !is_invisible_measure_name {
            return true;
        }
        fields.iter().enumerate().any(|(index, field)| {
            let guid = prev_order.get(index).map(String::as_str).unwrap_or("");
            match field
                .get("ref")
                .and_then(|reference| reference.get("type"))
                .and_then(Value::as_str)
            {
                Some("id") => {
                    field
                        .pointer("/ref/id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        != guid
                }
                Some("measure_name") => guid != "measure_name",
                _ => false,
            }
        })
    };
    let is_sort_supported = |meta: &Value| -> bool {
        // JS `meta.measureGuid` is falsy for the empty string, so it means "no specific
        // measure" for a single-measure pivot; treating `Some("")` as a real guid dropped the
        // whole sorting block (the preview click-sort regression).
        let measure_guid_meta = meta
            .get("measureGuid")
            .and_then(Value::as_str)
            .filter(|guid| !guid.is_empty());
        if measures_len > 1 {
            measure_guid_meta.map(|guid| guid == measure_guid).unwrap_or(false)
        } else if let Some(guid) = measure_guid_meta {
            guid == measure_guid
        } else {
            true
        }
    };
    let build_side = |meta: &Value, fields: &[Value], fallback_role: &str| -> Option<Value> {
        let is_empty = meta.as_object().map(|map| map.is_empty()).unwrap_or(true);
        if is_empty || fields_changed(meta, fields) {
            return None;
        }
        let next_direction = meta.get("nextSortDirection").and_then(Value::as_str)?;
        if !is_sort_supported(meta) {
            return None;
        }
        let header_values: Vec<Value> = meta
            .get("path")
            .and_then(Value::as_array)
            .map(|path| path.iter().map(|value| json!({"value": value})).collect())
            .unwrap_or_default();
        let role = meta
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(fallback_role);
        Some(json!({
            "header_values": header_values,
            "role_spec": {"role": role},
            "direction": next_direction,
        }))
    };
    let mut sorting = Map::new();
    if let Some(column_sorting) = build_side(&column, columns, "pivot_column") {
        sorting.insert("column".to_string(), column_sorting);
    }
    if let Some(row_sorting) = build_side(&row, rows, "pivot_row") {
        sorting.insert("row".to_string(), row_sorting);
    }
    if sorting.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(sorting)))
    }
}

/// The first hierarchy of a chart after `preprocessHierarchies`, i.e. `sharedData.drillDownData`.
struct DrillDown {
    breadcrumbs: Vec<String>,
    /// One filter value per hierarchy field (`drillDownFilters`, padded with `""`).
    filters: Vec<String>,
    level: usize,
    fields: Vec<Value>,
}

/// `getDrillDownData` (`utils/misc-helpers.ts:225-245`).
fn drill_down_params(params: &Value) -> (usize, Vec<String>) {
    let level = get_param(params, "drillDownLevel")
        .and_then(|value| number_value(&value))
        .filter(|level| level.is_finite() && *level > 0.0)
        .map(|level| level as usize)
        .unwrap_or(0);
    let filters: Vec<String> = match params.get("drillDownFilters") {
        Some(Value::Array(items)) => items.iter().map(js_string).collect(),
        Some(value) if !value.is_null() => vec![js_string(value)],
        _ => Vec::new(),
    };
    // `getDrillDownFilters` returns `undefined` when every value is empty, and
    // `preprocessHierarchies` then fills one empty value per hierarchy field.
    let filters = if filters.iter().any(|value| !value.is_empty()) {
        filters
    } else {
        Vec::new()
    };
    (level, filters)
}

/// `isFieldHierarchy` + `removeMarkupFieldsFromHierarchy` (`utils/hierarchy-helpers.ts`).
fn hierarchy_fields(item: &Value) -> Vec<Value> {
    if field_data_type(item) != "hierarchy" {
        return Vec::new();
    }
    item.get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter(|field| !is_markup_field(field))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn replace_hierarchy_with_level(item: &mut Value, fields: &[Value], level: usize) {
    // `hierarchyFieldIndex` (`hierarchy-helpers.ts:73-77`), clamped so a level equal to the
    // field count cannot index past the end.
    let index = if fields.len() < level {
        fields.len() - 1
    } else {
        level.min(fields.len() - 1)
    };
    *item = fields[index].clone();
}

/// `preprocessHierarchies` (`utils/hierarchy-helpers.ts`): each hierarchy pseudo-field is
/// replaced by its field for the current drill level, and the first one is returned as the
/// chart's `drillDownData` (breadcrumbs, per-level filters, current level).
///
/// The JS also records `sharedData.metaHierarchy` for the client; the server envelope has no
/// such member, so it is not built here.
fn preprocess_hierarchies(shared: &Value, params: &Value) -> (Value, Option<DrillDown>) {
    let mut expanded = shared.clone();
    let (level, filter_values) = drill_down_params(params);
    let mut drill: Option<DrillDown> = None;
    let apply = |items: &mut Vec<Value>, drill: &mut Option<DrillDown>| {
        for item in items.iter_mut() {
            let fields = hierarchy_fields(item);
            if fields.is_empty() {
                continue;
            }
            if drill.is_none() {
                *drill = Some(DrillDown {
                    breadcrumbs: fields
                        .iter()
                        .map(|field| {
                            field
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        })
                        .collect(),
                    filters: if filter_values.is_empty() {
                        vec![String::new(); fields.len()]
                    } else {
                        filter_values.clone()
                    },
                    level,
                    fields: fields.clone(),
                });
            }
            replace_hierarchy_with_level(item, &fields, level);
        }
    };
    if let Some(placeholders) = expanded
        .pointer_mut("/visualization/placeholders")
        .and_then(Value::as_array_mut)
    {
        for placeholder in placeholders {
            if let Some(items) = placeholder.get_mut("items").and_then(Value::as_array_mut) {
                apply(items, &mut drill);
            }
        }
    }
    for key in ["colors", "shapes", "segments"] {
        if let Some(items) = expanded.get_mut(key).and_then(Value::as_array_mut) {
            apply(items, &mut drill);
        }
    }
    (expanded, drill)
}

/// `formatFilters` drill-down part (`url/build-request-body/index.ts:248-281`): one `EQ`
/// filter per already-drilled level, tagged `isDrillDown`.
fn format_drill_filters(
    drill: Option<&DrillDown>,
    schema: &[Value],
    dataset: &str,
) -> Vec<Value> {
    let Some(drill) = drill else {
        return Vec::new();
    };
    let mut params = Map::new();
    for index in 0..drill.level.min(drill.filters.len()).min(drill.fields.len()) {
        let value = &drill.filters[index];
        if value.is_empty() {
            continue;
        }
        let Some(guid) = field_id(&drill.fields[index]) else {
            continue;
        };
        params.insert(guid.to_string(), json!(value));
    }
    if params.is_empty() {
        return Vec::new();
    }
    format_params_filters(&Value::Object(params), schema, dataset)
        .into_iter()
        .map(|(column, operation, values)| {
            json!({
                "ref": {"type": "id", "id": column},
                "operation": operation,
                "values": values,
                "isDrillDown": true,
            })
        })
        .collect()
}

/// Build the `POST /api/data/v2/datasets/{dataset}/pivot` body.
pub fn build_request(
    shared: &Value,
    params: &Value,
    dataset: &str,
    revision: &Value,
    dataset_fields: &Value,
) -> Result<Value, String> {
    let (expanded, drill) = preprocess_hierarchies(shared, params);
    let shared = &expanded;
    let visualization = shared
        .get("visualization")
        .ok_or_else(|| "no visualization".to_string())?;
    let columns = placeholder_items(shared, "pivot-table-columns");
    let rows = placeholder_items(shared, "rows");
    let measures = placeholder_items(shared, "measures");

    let order_by_pairs = table_order_by(shared, params);
    let direction_for = |guid: &str| -> String {
        order_by_pairs
            .iter()
            .find(|(column, _)| column == guid)
            .map(|(_, direction)| direction.to_lowercase())
            .unwrap_or_else(|| "asc".to_string())
    };

    let mut fields: Vec<Value> = Vec::new();
    let mut structure: Vec<Value> = Vec::new();
    let mut used_fields_map: HashMap<String, (i64, String)> = HashMap::new();
    let mut used_legend_item_ids: HashSet<i64> = HashSet::new();
    let mut legend_counter: i64 = 0;

    for item in &columns {
        let legend_item_id = legend_counter;
        legend_counter += 1;
        if is_measure_name(item) {
            fields.push(json!({
                "legend_item_id": legend_item_id,
                "block_id": 0,
                "ref": {"type": "measure_name"},
            }));
            structure.push(json!({
                "legend_item_ids": [legend_item_id],
                "role_spec": {"role": "pivot_column", "direction": "asc"},
            }));
            used_legend_item_ids.insert(legend_item_id);
            continue;
        }
        let guid = field_id(item).unwrap_or("").to_string();
        fields.push(json!({
            "legend_item_id": legend_item_id,
            "block_id": 0,
            "ref": {"type": "id", "id": guid},
        }));
        structure.push(json!({
            "legend_item_ids": [legend_item_id],
            "role_spec": {"role": "pivot_column", "direction": direction_for(&guid)},
        }));
        used_fields_map.insert(guid, (legend_item_id, "pivot_column".to_string()));
        used_legend_item_ids.insert(legend_item_id);
    }
    for item in &rows {
        let legend_item_id = legend_counter;
        legend_counter += 1;
        // `getRegularFields` (rows): Measure Names *and* Measure Values are sent as
        // `measure_name`; the old summary allowed Measure Values in the rows.
        if is_measure_name(item) || is_measure_value(item) {
            fields.push(json!({
                "legend_item_id": legend_item_id,
                "block_id": 0,
                "ref": {"type": "measure_name"},
            }));
            structure.push(json!({
                "legend_item_ids": [legend_item_id],
                "role_spec": {"role": "pivot_row", "direction": "asc"},
            }));
            used_legend_item_ids.insert(legend_item_id);
            continue;
        }
        let guid = field_id(item).unwrap_or("").to_string();
        fields.push(json!({
            "legend_item_id": legend_item_id,
            "block_id": 0,
            "ref": {"type": "id", "id": guid},
        }));
        structure.push(json!({
            "legend_item_ids": [legend_item_id],
            "role_spec": {"role": "pivot_row", "direction": direction_for(&guid)},
        }));
        used_fields_map.insert(guid, (legend_item_id, "pivot_row".to_string()));
        used_legend_item_ids.insert(legend_item_id);
    }
    for item in &measures {
        let guid = field_id(item).unwrap_or("").to_string();
        let legend_item_id = legend_counter;
        legend_counter += 1;
        fields.push(json!({
            "legend_item_id": legend_item_id,
            "block_id": 0,
            "ref": {"type": "id", "id": guid},
        }));
        let mut role_spec = Map::new();
        role_spec.insert("role".to_string(), json!("pivot_measure"));
        role_spec.insert("direction".to_string(), json!(direction_for(&guid)));
        if let Some(sorting) =
            measure_sorting(params, &guid, &columns, &rows, measures.len())?
        {
            role_spec.insert("sorting".to_string(), sorting);
        }
        structure.push(json!({
            "legend_item_ids": [legend_item_id],
            "role_spec": Value::Object(role_spec),
        }));
        used_fields_map.insert(guid, (legend_item_id, "pivot_measure".to_string()));
        used_legend_item_ids.insert(legend_item_id);
    }

    // Annotations from `shared.colors`.
    for item in shared
        .get("colors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if is_measure_name(&item) {
            continue;
        }
        let guid = field_id(&item).unwrap_or("").to_string();
        let direction = direction_for(&guid);
        let legend_item_id = used_fields_map
            .get(&guid)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| {
                let id = legend_counter;
                legend_counter += 1;
                id
            });
        let missed_in_request =
            !used_fields_map.contains_key(&guid) && !used_legend_item_ids.contains(&legend_item_id);
        if missed_in_request {
            fields.push(json!({
                "legend_item_id": legend_item_id,
                "block_id": 0,
                "ref": {"type": "id", "id": guid},
            }));
        }
        structure.push(json!({
            "legend_item_ids": [legend_item_id],
            "role_spec": {
                "role": "pivot_annotation",
                "direction": direction,
                "annotation_type": "color",
            },
        }));
    }

    // `getAnnotations` (`pivot-request/helpers/index.ts:143-190`): per-field background
    // colouring is requested as a `background-color` annotation on the measure field.
    for item in all_placeholder_items(shared) {
        if is_parameter(item)
            || !item
                .pointer("/backgroundSettings/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let Some(target_guid) = field_id(item).map(str::to_string) else {
            continue;
        };
        let Some((target_legend_item_id, role)) = used_fields_map.get(&target_guid) else {
            continue;
        };
        // The reference only requests annotations for measure targets.
        if role != "pivot_measure" {
            continue;
        }
        let background = item
            .get("backgroundSettings")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let color_field_guid = background
            .get("colorFieldGuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let is_continuous = background
            .pointer("/settings/isContinuous")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // `TODO: CHARTS-7124`: the backend cannot annotate `Measure Names`.
        if color_field_guid.is_empty()
            || color_field_guid == MEASURE_NAMES
            || (!is_continuous && !used_fields_map.contains_key(&color_field_guid))
        {
            continue;
        }
        let legend_item_id = used_fields_map
            .get(&color_field_guid)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| {
                let id = legend_counter;
                legend_counter += 1;
                id
            });
        let missed_in_request = !used_fields_map.contains_key(&color_field_guid)
            && !used_legend_item_ids.contains(&legend_item_id);
        if missed_in_request {
            fields.push(json!({
                "legend_item_id": legend_item_id,
                "block_id": 0,
                "ref": {"type": "id", "id": color_field_guid},
            }));
        }
        structure.push(json!({
            "legend_item_ids": [legend_item_id],
            "role_spec": {
                "role": "pivot_annotation",
                "annotation_type": "background-color",
                "target_legend_item_ids": [target_legend_item_id],
            },
        }));
    }

    let schema = merge_dataset_schema(dataset_fields, shared);
    let filters_params = format_params_filters(params, &schema, dataset);
    let chart_filters = format_chart_filters(shared, dataset, &schema)?;
    let mut filters = merge_filters(chart_filters, filters_params, params, &schema);
    filters.extend(format_drill_filters(drill.as_ref(), &schema, dataset));

    let updates: Vec<Value> = shared
        .get("updates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|update| {
            update
                .pointer("/field/datasetId")
                .and_then(Value::as_str)
                .map(|update_dataset| update_dataset == dataset)
                .unwrap_or(true)
        })
        .collect();

    let order_by: Vec<Value> = order_by_pairs
        .iter()
        .map(|(column, direction)| {
            json!({
                "ref": {"type": "id", "id": column},
                "direction": direction.to_lowercase(),
            })
        })
        .collect();

    // `getPayloadWithCommonTableSettings`: a paginated pivot sends the page offset/limit both
    // inside `pivot.pagination` and as the top-level `pivot_pagination`.
    let extra = shared
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pagination_on =
        extra.get("pagination").and_then(Value::as_str) == Some("on");
    let limit = extra.get("limit").and_then(number_value);
    let mut pagination = Map::new();
    if pagination_on {
        if let Some(limit) = limit {
            let page = get_current_page(params);
            let page = if page.is_finite() && page >= 1.0 { page } else { 1.0 };
            let offset_rows = ((page - 1.0) * limit) as i64;
            let limit_rows = limit as i64;
            pagination.insert("offset_rows".to_string(), json!(offset_rows));
            pagination.insert("limit_rows".to_string(), json!(limit_rows));
        }
    }
    let pagination = Value::Object(pagination);
    let totals = pivot_sub_totals(shared);
    let mut body = Map::new();
    body.insert("fields".to_string(), Value::Array(fields));
    body.insert(
        "pivot".to_string(),
        json!({
            "structure": structure,
            "pagination": pagination,
            "totals": totals,
        }),
    );
    body.insert("filters".to_string(), Value::Array(filters));
    body.insert("order_by".to_string(), Value::Array(order_by));
    body.insert("parameter_values".to_string(), json!([]));
    body.insert("pivot_pagination".to_string(), pagination);
    body.insert("dataset_revision_id".to_string(), revision.clone());
    body.insert("updates".to_string(), Value::Array(updates));
    body.insert("ignore_nonexistent_filters".to_string(), json!(true));
    body.insert("autofill_legend".to_string(), json!(true));
    let _ = visualization;
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// `data` preparation (preparers/backend-pivot-table)
// ---------------------------------------------------------------------------

struct PivotContext<'a> {
    shared: &'a Value,
    response: &'a Value,
    fields_by_legend: HashMap<i64, Value>,
    field_dict: HashMap<String, Value>,
    lang: &'a str,
}

fn field_legend_id(field: &Value) -> Option<i64> {
    field.get("legend_item_id").and_then(Value::as_i64)
}

fn build_context<'a>(
    shared: &'a Value,
    response: &'a Value,
    lang: &'a str,
) -> Result<PivotContext<'a>, String> {
    let fields = response
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| "pivot response has no fields".to_string())?;
    let mut fields_by_legend = HashMap::new();
    let mut id_to_title: HashMap<String, String> = HashMap::new();
    let mut id_to_data_type: HashMap<String, String> = HashMap::new();
    for field in fields {
        if let Some(legend) = field_legend_id(field) {
            fields_by_legend.insert(legend, field.clone());
        }
        if let Some(guid) = field_id(field) {
            if let Some(title) = field.get("title").and_then(Value::as_str) {
                id_to_title.insert(guid.to_string(), title.to_string());
            }
            if let Some(data_type) = field.get("data_type").and_then(Value::as_str) {
                id_to_data_type.insert(guid.to_string(), data_type.to_string());
            }
        }
        if field.get("item_type").and_then(Value::as_str) == Some("placeholder") {
            // A totals (sub-total / grand-total) block cell: `role_spec.role == "template"`.
            // The JS preparer keeps it and paints it as a total cell.
            continue;
        }
    }
    let mut field_dict: HashMap<String, Value> = HashMap::new();
    if let Some(placeholders) = shared
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)
    {
        for placeholder in placeholders {
            let Some(items) = placeholder.get("items").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let Some(guid) = field_id(item).map(str::to_string) else {
                    continue;
                };
                let mut actualized = item.clone();
                if let Some(map) = actualized.as_object_mut() {
                    if let Some(data_type) = id_to_data_type.get(&guid) {
                        map.insert("data_type".to_string(), json!(data_type));
                    }
                    if let Some(title) = id_to_title.get(&guid) {
                        map.insert("title".to_string(), json!(title));
                    }
                }
                field_dict.insert(guid, actualized.clone());
                if let Some(title) = actualized.get("title").and_then(Value::as_str) {
                    field_dict.insert(title.to_string(), actualized);
                }
            }
        }
    }
    Ok(PivotContext {
        shared,
        response,
        fields_by_legend,
        field_dict,
        lang,
    })
}

fn cell_identity(ctx: &PivotContext<'_>, cell_vector: &Value) -> Option<(Value, Value, i64)> {
    let first = cell_vector.get(0)?;
    let value = first.get(0).cloned().unwrap_or(Value::Null);
    let legend = first.get(1).and_then(Value::as_i64)?;
    let pivot_item_id = first.get(2).and_then(Value::as_i64).unwrap_or(0);
    Some((value, ctx.fields_by_legend.get(&legend)?.clone(), pivot_item_id))
}

fn dataset_field_from_cell(ctx: &PivotContext<'_>, cell_vector: &Value) -> Option<Value> {
    let first = cell_vector.get(0)?;
    let legend = first.get(1).and_then(Value::as_i64)?;
    let field = ctx.fields_by_legend.get(&legend)?.clone();
    let is_measure_name_item =
        field.get("item_type").and_then(Value::as_str) == Some("measure_name");
    let guid = if is_measure_name_item {
        first
            .get(0)
            .map(js_string)
            .unwrap_or_default()
            .trim_start_matches("title-")
            .to_string()
    } else {
        field_id(&field).unwrap_or("").to_string()
    };
    ctx.field_dict.get(&guid).cloned()
}

fn get_cell_value_for_header(value: &Value, dataset_field: Option<&Value>) -> Value {
    if let Some(field) = dataset_field {
        if is_markup_field(field) || is_markup_item(value) {
            return Value::String(js_string(value));
        }
    }
    value.clone()
}

fn annotations_map(structure: &Value) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    let Some(items) = structure.as_array() else {
        return map;
    };
    for item in items {
        if item.pointer("/role_spec/role").and_then(Value::as_str) != Some("pivot_annotation") {
            continue;
        }
        if let (Some(annotation_type), Some(pivot_item_id)) = (
            item.pointer("/role_spec/annotation_type")
                .and_then(Value::as_str),
            item.get("pivot_item_id").and_then(Value::as_i64),
        ) {
            map.insert(pivot_item_id, annotation_type.to_string());
        }
    }
    map
}

fn get_annotation<'a>(
    cell_vector: &'a Value,
    annotations: &HashMap<i64, String>,
    annotation: &str,
) -> Option<&'a Value> {
    cell_vector.as_array()?.iter().find(|cell| {
        let pivot_item_id = cell.get(2).and_then(Value::as_i64).unwrap_or(-1);
        annotations.get(&pivot_item_id).map(String::as_str) == Some(annotation)
    })
}

fn get_sort_direction(current: Option<&str>) -> Option<&'static str> {
    match current {
        Some("desc") => Some("asc"),
        Some("asc") => None,
        None => Some("desc"),
        _ => None,
    }
}

/// Sort metadata object used both for the head `custom` cell and, serialized
/// with `JSON.stringify` key order, for the row `onClick._sortRowMeta` string.
fn sort_meta_parts(
    meta: &Value,
    path: Value,
    measure_guid: &str,
    field_order: Value,
) -> (Value, Option<String>) {
    let role = meta
        .pointer("/role_spec/role")
        .and_then(Value::as_str)
        .unwrap_or("");
    let current = meta.get("sorting_direction").and_then(Value::as_str);
    let next = get_sort_direction(current);
    let mut object = Map::new();
    object.insert("role".to_string(), json!(role));
    object.insert(
        "currentSortDirection".to_string(),
        current.map(|value| json!(value)).unwrap_or(Value::Null),
    );
    if let Some(next) = next {
        object.insert("nextSortDirection".to_string(), json!(next));
    }
    object.insert("path".to_string(), path);
    object.insert("measureGuid".to_string(), json!(measure_guid));
    object.insert("fieldOrder".to_string(), field_order);
    (Value::Object(object), current.map(str::to_string))
}

/// `JSON.stringify(sortMeta)` with the JS insertion order of `getSortMeta`.
fn sort_meta_json_string(
    meta: &Value,
    path: &Value,
    measure_guid: &str,
    field_order: &Value,
) -> String {
    let role = meta
        .pointer("/role_spec/role")
        .and_then(Value::as_str)
        .unwrap_or("");
    let current = meta.get("sorting_direction").and_then(Value::as_str);
    let next = get_sort_direction(current);
    let mut out = String::from("{");
    out.push_str(&format!("\"role\":{}", json!(role)));
    out.push_str(&format!(
        ",\"currentSortDirection\":{}",
        current.map(|value| json!(value)).unwrap_or(Value::Null)
    ));
    if let Some(next) = next {
        out.push_str(&format!(",\"nextSortDirection\":{}", json!(next)));
    }
    out.push_str(&format!(",\"path\":{}", serde_json::to_string(path).unwrap_or_default()));
    out.push_str(&format!(",\"measureGuid\":{}", json!(measure_guid)));
    out.push_str(&format!(
        ",\"fieldOrder\":{}",
        serde_json::to_string(field_order).unwrap_or_default()
    ));
    out.push('}');
    out
}

fn get_row_header_cell_metadata(
    ctx: &PivotContext<'_>,
    cell: &Value,
    is_total_header: bool,
) -> Result<Value, String> {
    let Some(components) = cell.as_array() else {
        return Ok(json!({"value": cell.clone()}));
    };
    let value = components.first().cloned().unwrap_or(Value::Null);
    let legend_item_id = components.get(1).and_then(Value::as_i64).unwrap_or(-1);
    let pivot_field = ctx
        .fields_by_legend
        .get(&legend_item_id)
        .cloned()
        .unwrap_or_else(|| json!({}));
    let is_measure_name_item =
        pivot_field.get("id").and_then(Value::as_str) == Some(MEASURE_NAME_PSEUDO_ID);
    let field_guid_or_title = if is_measure_name_item {
        js_string(&value).trim_start_matches("title-").to_string()
    } else {
        field_id(&pivot_field).unwrap_or("").to_string()
    };
    let dataset_field = ctx.field_dict.get(&field_guid_or_title).cloned();
    let mut result = Map::new();
    result.insert("value".to_string(), value.clone());
    // JS insertion order for row-dimension header cells: value, css, isTotalCell, ...
    result.insert(
        "css".to_string(),
        if is_total_header {
            table_totals_styles()
        } else {
            json!({})
        },
    );
    result.insert("isTotalCell".to_string(), json!(is_total_header));
    let Some(dataset_field) = dataset_field else {
        return Ok(Value::Object(result));
    };
    if is_measure_name_item {
        let fake_title = dataset_field
            .get("fakeTitle")
            .and_then(Value::as_str)
            .unwrap_or("");
        result.insert(
            "value".to_string(),
            json!(if fake_title.is_empty() {
                js_string(&value)
            } else {
                fake_title.to_string()
            }),
        );
        return Ok(Value::Object(result));
    }
    if is_number_field(&dataset_field) {
        result.insert("type".to_string(), json!("number"));
        // JS `getRowHeaderCellMetadata`: type, then fieldId, then formattedValue.
        if let Some(guid) = field_id(&dataset_field) {
            result.insert("fieldId".to_string(), json!(guid));
        }
        if !value.is_null() {
            let number = number_value(&value).unwrap_or(0.0);
            result.insert("value".to_string(), json_number(number));
            result.insert(
                "formattedValue".to_string(),
                json!(chart_kit_format_number(number, &dataset_field)?),
            );
        }
    } else if is_date_field(&dataset_field) && js_truthy(&value) {
        result.insert(
            "formattedValue".to_string(),
            json!(format_date_value(
                field_data_type(&dataset_field),
                &value,
                dataset_field.get("format").and_then(Value::as_str),
            )?),
        );
    }
    Ok(Value::Object(result))
}

fn color_annotation_value(cell_vector: &Value, annotations: &HashMap<i64, String>) -> Option<f64> {
    let annotation = get_annotation(cell_vector, annotations, "color")?;
    let color_value = annotation.get(0).cloned().unwrap_or(Value::Null);
    match &color_value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        other => number_value(other),
    }
}

/// `getPivotTableSubTotals` (`utils/pivot-table/totals.ts`): one `{level: index}` per
/// placeholder item with `subTotalsSettings.enabled`.
fn pivot_sub_totals(shared: &Value) -> Value {
    let mut totals = Map::new();
    for (placeholder_id, key) in [("pivot-table-columns", "columns"), ("rows", "rows")] {
        let levels: Vec<Value> = placeholder_items(shared, placeholder_id)
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.pointer("/subTotalsSettings/enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .map(|(index, _)| json!({"level": index as i64}))
            .collect();
        totals.insert(key.to_string(), Value::Array(levels));
    }
    Value::Object(totals)
}

/// `isTotalsEnabled` (`backend-pivot-table/index.ts:147`).
fn is_totals_enabled(shared: &Value) -> bool {
    all_placeholder_items(shared).iter().any(|item| {
        item.pointer("/subTotalsSettings/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
}

fn is_placeholder_field(field: &Value) -> bool {
    field.get("item_type").and_then(Value::as_str) == Some("placeholder")
}

/// `isRowWithTotals` (`helpers/totals.ts:5`): any header cell points at a placeholder field.
fn is_row_with_totals(ctx: &PivotContext<'_>, header: &[Value]) -> bool {
    header.iter().any(|header_cell| {
        header_cell
            .as_array()
            .and_then(|vector| vector.first())
            .and_then(|first| first.get(1))
            .and_then(Value::as_i64)
            .and_then(|legend| ctx.fields_by_legend.get(&legend))
            .map(is_placeholder_field)
            .unwrap_or(false)
    })
}

/// `headerTotalsIndexMap` (`backend-pivot-table/index.ts:59-76`): the column indexes whose
/// cells contain a placeholder field are totals columns.
fn header_totals_index_map(ctx: &PivotContext<'_>, columns_with_info: &[Value]) -> HashSet<usize> {
    let mut map = HashSet::new();
    for (index, column) in columns_with_info.iter().enumerate() {
        let has_totals = column
            .get("cells")
            .and_then(Value::as_array)
            .map(|cells| {
                cells.iter().any(|cell| {
                    cell.as_array()
                        .and_then(|vector| vector.first())
                        .and_then(|first| first.get(1))
                        .and_then(Value::as_i64)
                        .and_then(|legend| ctx.fields_by_legend.get(&legend))
                        .map(is_placeholder_field)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if has_totals {
            map.insert(index);
        }
    }
    map
}

/// `wizard.prepares:label_total` for the chart language (`i18n-keysets/wizard.prepares`).
/// Unknown languages keep the Russian label (the engine's number formatting is `ru`).
fn label_total(lang: &str) -> &'static str {
    if lang == "en" {
        "Total"
    } else {
        "Итого"
    }
}

/// `setTotalsHeaders` (`helpers/totals.ts:80`). `label_total` follows the chart `lang`.
fn set_totals_headers(rows: &mut [Value], head: &mut Value, lang: &str) {
    let total_title = label_total(lang);
    if let Some(head) = head.as_array_mut() {
        set_totals_headers_to_head(head, total_title, None);
    }
    for row in rows.iter_mut() {
        let Some(cells) = row.get_mut("cells").and_then(Value::as_array_mut) else {
            continue;
        };
        // `isTotalHeaderSet`: only the first empty total cell of a row is labelled.
        if let Some(index) = cells.iter().position(|cell| {
            cell.get("isTotalCell").and_then(Value::as_bool).unwrap_or(false)
                && cell.get("value").and_then(Value::as_str) == Some("")
        }) {
            let previous = index.checked_sub(1).and_then(|prev| cells.get(prev));
            let (value, is_markup) = match previous {
                Some(previous) => {
                    let previous_value = previous.get("value").cloned().unwrap_or(Value::Null);
                    if is_markup_item(&previous_value) {
                        (
                            json!({
                                "type": "concat",
                                "children": [
                                    {"type": "text", "content": format!("{total_title} ")},
                                    previous_value,
                                ],
                            }),
                            true,
                        )
                    } else {
                        (
                            json!(format!("{total_title} {}", js_string(&previous_value))),
                            false,
                        )
                    }
                }
                None => (json!(total_title), false),
            };
            let cell = &mut cells[index];
            cell["value"] = value;
            if !is_markup {
                cell["type"] = json!("text");
            }
        }
    }
}

/// `setTotalsHeadersToHead` (`helpers/totals.ts:51`): the first total cell of a level is named
/// `Total <parent name>`; the walk continues into every non-total item's children.
fn set_totals_headers_to_head(head: &mut [Value], total_title: &str, parent_name: Option<&Value>) {
    for item in head.iter_mut() {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if object
            .get("isTotalCell")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let name = match parent_name.filter(|parent| js_truthy(parent)) {
                Some(parent) => {
                    let parent = if is_markup_item(parent) {
                        markup_to_raw_string(parent)
                    } else {
                        js_string(parent)
                    };
                    json!(format!("{total_title} {parent}"))
                }
                None => json!(total_title),
            };
            object.insert("name".to_string(), name);
            return;
        }
        let child_name = object.get("name").cloned();
        if let Some(children) = object.get_mut("sub").and_then(Value::as_array_mut) {
            set_totals_headers_to_head(children, total_title, child_name.as_ref());
        }
    }
}

/// `getGrandTotalsRowIndex` (`helpers/totals.ts:90`): totals are always the trailing rows.
fn get_grand_totals_row_index(rows: &[Value]) -> Option<usize> {
    let is_total = |row: &Value| {
        row.pointer("/cells/0/isTotalCell")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    for index in (0..rows.len()).rev() {
        if index + 1 < rows.len() && is_total(&rows[index + 1]) && !is_total(&rows[index]) {
            return Some(index + 1);
        }
    }
    None
}

/// `markupToRawString`: the concatenated text content of a markup item.
fn markup_to_raw_string(item: &Value) -> String {
    if let Some(content) = item.get("content").and_then(Value::as_str) {
        return content.to_string();
    }
    item.get("children")
        .and_then(Value::as_array)
        .map(|children| {
            children
                .iter()
                .map(markup_to_raw_string)
                .collect::<Vec<_>>()
                .concat()
        })
        .unwrap_or_default()
}

fn is_sort_by_role_allowed(pivot_structure: &Value, pivot_totals: &Value, role: &str) -> bool {
    let has_subtotals = |key: &str| -> bool {
        pivot_totals
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .any(|item| item.get("level").and_then(Value::as_i64).unwrap_or(0) > 0)
            })
            .unwrap_or(false)
    };
    let has_row_subtotals = has_subtotals("rows");
    let has_column_subtotals = has_subtotals("columns");
    if (has_row_subtotals && role == "pivot_column")
        || (has_column_subtotals && role == "pivot_row")
    {
        return false;
    }
    let measures = pivot_structure
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.pointer("/role_spec/role").and_then(Value::as_str)
                        == Some("pivot_measure")
                })
                .count()
        })
        .unwrap_or(0);
    if measures > 1 {
        return pivot_structure
            .as_array()
            .map(|items| {
                items.iter().any(|item| {
                    item.get("title").and_then(Value::as_str) == Some(MEASURE_NAMES)
                        && item.pointer("/role_spec/role").and_then(Value::as_str) == Some(role)
                })
            })
            .unwrap_or(false);
    }
    true
}

fn generate_table_rows(ctx: &PivotContext<'_>) -> Result<Vec<Value>, String> {
    let extra = ctx
        .shared
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let is_inline_sort_enabled =
        extra.get("pivotInlineSort").and_then(Value::as_str) != Some("off");
    let pivot_structure = ctx
        .response
        .pointer("/pivot/structure")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let annotations = annotations_map(&pivot_structure);
    let pivot_totals = pivot_sub_totals(ctx.shared);
    let is_sort_by_row_allowed =
        is_sort_by_role_allowed(&pivot_structure, &pivot_totals, "pivot_row")
            && is_inline_sort_enabled;

    let pivot_data = ctx
        .response
        .get("pivot_data")
        .ok_or_else(|| "pivot response has no pivot_data".to_string())?;
    let rows_data = pivot_data
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_totals_enabled = is_totals_enabled(ctx.shared);
    let header_totals = header_totals_index_map(
        ctx,
        &pivot_data
            .get("columns_with_info")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    );

    let values_by_field = pivot_values_by_field(ctx, &rows_data);
    let bars = pivot_bars_by_field(&ctx.field_dict, &values_by_field);

    let mut rows: Vec<Value> = Vec::new();
    let mut cell_id: i64 = 0;
    for row_data in &rows_data {
        let header = row_data
            .pointer("/header_with_info/cells")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let is_totals_row = is_totals_enabled && is_row_with_totals(ctx, &header);
        let header_info = row_data
            .pointer("/header_with_info/header_info")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let values = row_data
            .get("values")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut cells: Vec<Value> = Vec::new();
        let mut header_parent_by_index: Vec<Value> = Vec::new();
        let mut measure_guid = String::new();
        let mut field_order: Vec<Value> = Vec::new();
        let mut is_header_contains_markup = false;
        for (header_index, pivot_data_row_value) in header.iter().enumerate() {
            if !js_truthy(pivot_data_row_value) {
                cells.push(json!({"value": ""}));
                continue;
            }
            let cell_vector = pivot_data_row_value
                .as_array()
                .cloned()
                .ok_or_else(|| "malformed pivot header".to_string())?;
            let dataset_field = dataset_field_from_cell(ctx, pivot_data_row_value);
            let is_measure = dataset_field
                .as_ref()
                .map(is_measure_field)
                .unwrap_or(false);
            if let Some(dataset_field) = &dataset_field {
                if measure_guid.is_empty() {
                    measure_guid = if is_measure {
                        field_id(dataset_field).unwrap_or("").to_string()
                    } else {
                        String::new()
                    };
                }
                field_order.push(json!(if is_measure {
                    "measure_name"
                } else {
                    field_id(dataset_field).unwrap_or("")
                }));
            }
            let path = header_parent_by_index.clone();
            let first_cell = cell_vector
                .first()
                .cloned()
                .ok_or_else(|| "empty row header vector".to_string())?;
            let raw_value = first_cell
                .get(0)
                .cloned()
                .unwrap_or(Value::Null);
            let mut cell = Map::new();
            match dataset_field.as_ref().and_then(field_id) {
                Some(guid) => {
                    cell.insert(
                        "id".to_string(),
                        json!(format!("fieldId={guid}__index={cell_id}")),
                    );
                }
                None => {
                    cell.insert("id".to_string(), json!(format!("fieldId=__index={cell_id}")));
                }
            }
            cell_id += 1;
            // `table-rows-generator.ts:242`: the first empty cell of a totals row is the
            // "Total" label cell.
            let is_total_header = is_totals_row && raw_value.as_str() == Some("");
            let metadata = get_row_header_cell_metadata(ctx, &first_cell, is_total_header)?;
            if let Some(metadata_map) = metadata.as_object() {
                for (key, value) in metadata_map {
                    cell.insert(key.clone(), value.clone());
                }
            }
            header_parent_by_index.push(get_cell_value_for_header(
                &raw_value,
                dataset_field.as_ref(),
            ));
            if is_markup_item(&raw_value) {
                is_header_contains_markup = true;
            }
            let is_last_header = header_index == header.len() - 1;
            let is_sorting_allowed = is_sort_by_row_allowed
                && dataset_field.is_some()
                && is_last_header
                && !is_header_contains_markup;
            if is_sorting_allowed {
                let current_value = cell.get("value").cloned().unwrap_or(Value::Null);
                if current_value.is_null() {
                    cell.insert(
                        "onClick".to_string(),
                        json!({
                            "action": "showMsg",
                            "args": {"message": "label_null-sorting-disabled-info"},
                        }),
                    );
                } else {
                    let mut path_value = path.clone();
                    path_value.push(raw_value.clone());
                    let (sort_meta, sort_direction) = sort_meta_parts(
                        &header_info,
                        Value::Array(path_value.clone()),
                        &measure_guid,
                        Value::Array(field_order.clone()),
                    );
                    cell.insert(
                        "sortDirection".to_string(),
                        sort_direction
                            .map(|direction| json!(direction))
                            .unwrap_or(Value::Null),
                    );
                    let sort_string = sort_meta_json_string(
                        &header_info,
                        &Value::Array(path_value),
                        &measure_guid,
                        &Value::Array(field_order.clone()),
                    );
                    let _ = sort_meta;
                    cell.insert(
                        "onClick".to_string(),
                        json!({
                            "action": "setParams",
                            "args": {"_sortRowMeta": sort_string},
                        }),
                    );
                }
                let mut css = cell
                    .get("css")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                css.insert("cursor".to_string(), json!("pointer"));
                cell.insert("css".to_string(), Value::Object(css));
            }
            cells.push(Value::Object(cell));
        }

        for (value_index, pivot_data_row_value) in values.iter().enumerate() {
            if !js_truthy(pivot_data_row_value) {
                if is_totals_row || header_totals.contains(&value_index) {
                    cells.push(json!({"value": "", "css": table_totals_styles()}));
                } else {
                    cells.push(json!({"value": ""}));
                }
                continue;
            }
            let cell_vector = pivot_data_row_value
                .as_array()
                .cloned()
                .ok_or_else(|| "malformed pivot value".to_string())?;
            let dataset_field = dataset_field_from_cell(ctx, pivot_data_row_value);
            let mut cell = Map::new();
            match dataset_field.as_ref().and_then(field_id) {
                Some(guid) => {
                    cell.insert(
                        "id".to_string(),
                        json!(format!("fieldId={guid}__index={cell_id}")),
                    );
                    cell.insert("fieldId".to_string(), json!(guid));
                }
                None => {
                    cell.insert("id".to_string(), json!(format!("fieldId=__index={cell_id}")));
                }
            }
            cell_id += 1;
            let metadata = get_row_cell_metadata(
                ctx,
                &Value::Array(cell_vector),
                &annotations,
                dataset_field.as_ref(),
                &bars,
            )?;
            if let Some(metadata_map) = metadata.as_object() {
                for (key, value) in metadata_map {
                    cell.insert(key.clone(), value.clone());
                }
            }
            cells.push(Value::Object(cell));
        }
        rows.push(json!({"cells": cells}));
    }

    let colors = ctx
        .shared
        .get("colors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    colorize_rows_by_field_background(ctx, &mut rows, &rows_data, &annotations)?;
    colorize_rows_by_color_field(ctx, &mut rows, &rows_data, &annotations, &colors)?;
    Ok(rows)
}

fn get_row_cell_metadata(
    ctx: &PivotContext<'_>,
    cell_vector: &Value,
    annotations: &HashMap<i64, String>,
    dataset_field: Option<&Value>,
    bars: &HashMap<String, PivotBars>,
) -> Result<Value, String> {
    let (value, pivot_field, _) = cell_identity(ctx, cell_vector)
        .ok_or_else(|| "pivot value cell has no field".to_string())?;
    let is_totals_row_value =
        pivot_field.pointer("/role_spec/role").and_then(Value::as_str) == Some("total");
    let color_key = if let Some(annotation) = get_annotation(cell_vector, annotations, "background-color") {
        annotation.get(0).cloned().unwrap_or(Value::Null)
    } else {
        value.clone()
    };
    let mut cell = Map::new();
    cell.insert("value".to_string(), value.clone());
    cell.insert("colorKey".to_string(), color_key);
    cell.insert("isTotalCell".to_string(), json!(is_totals_row_value));
    cell.insert(
        "css".to_string(),
        if is_totals_row_value {
            table_totals_styles()
        } else {
            json!({})
        },
    );
    if value.is_null() {
        return Ok(Value::Object(cell));
    }
    let field = dataset_field.cloned().or_else(|| {
        let (_, field, _) = cell_identity(ctx, cell_vector)?;
        let guid = field_id(&field).unwrap_or("").to_string();
        ctx.field_dict.get(&guid).cloned()
    });
    let Some(field) = field else {
        return Ok(Value::Object(cell));
    };
    let bars_for_field = {
        let guid = field_id(&field).unwrap_or("");
        bars.get(guid).or_else(|| {
            field
                .get("title")
                .and_then(Value::as_str)
                .and_then(|title| bars.get(title))
        })
    };
    if is_markup_field(&field) {
        cell.insert("type".to_string(), json!("markup"));
    } else if is_number_field(&field) {
        cell.insert("type".to_string(), json!("number"));
        if let Some(bars_settings) = bars_for_field {
            let (bar_value, formatted_value, bar_color, show_bar) =
                bar_cell_settings(&field, &value, bars_settings, is_totals_row_value);
            cell.insert("value".to_string(), bar_value);
            cell.insert("formattedValue".to_string(), json!(formatted_value));
            if let Some(bar_color) = bar_color {
                cell.insert("barColor".to_string(), json!(bar_color));
            }
            cell.insert("showBar".to_string(), json!(show_bar));
            for (key, option) in &bars_settings.options {
                cell.insert(key.clone(), option.clone());
            }
            return Ok(Value::Object(cell));
        }
        let number = number_value(&value).unwrap_or(0.0);
        cell.insert("value".to_string(), json_number(number));
        cell.insert(
            "formattedValue".to_string(),
            json!(chart_kit_format_number(number, &field)?),
        );
    } else if is_date_field(&field) {
        cell.insert("type".to_string(), json!("date"));
        cell.insert(
            "formattedValue".to_string(),
            json!(format_date_value(
                field_data_type(&field),
                &value,
                field.get("format").and_then(Value::as_str),
            )?),
        );
    }
    Ok(Value::Object(cell))
}

fn colorize_rows_by_color_field(
    ctx: &PivotContext<'_>,
    rows: &mut [Value],
    rows_data: &[Value],
    annotations: &HashMap<i64, String>,
    colors: &[Value],
) -> Result<(), String> {
    let filtered_colors: Vec<&Value> = colors
        .iter()
        .filter(|color| !is_measure_name(color))
        .collect();
    if filtered_colors.is_empty() || rows_data.is_empty() {
        return Ok(());
    }
    let colors_config = ctx
        .shared
        .get("colorsConfig")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut color_values_by_row: Vec<Vec<Option<f64>>> = Vec::new();
    let mut flat: Vec<f64> = Vec::new();
    for row in rows_data {
        let values = row
            .get("values")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut row_values = Vec::new();
        for cell in &values {
            if !js_truthy(cell) {
                row_values.push(None);
                continue;
            }
            match color_annotation_value(cell, annotations) {
                Some(value) => {
                    flat.push(value);
                    row_values.push(Some(value));
                }
                None => row_values.push(None),
            }
        }
        color_values_by_row.push(row_values);
    }
    let (min, max) = if flat.is_empty() {
        (0.0, 0.0)
    } else {
        (
            flat.iter().copied().fold(f64::INFINITY, f64::min),
            flat.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        )
    };
    let row_header_length = rows_data
        .first()
        .and_then(|row| row.pointer("/header_with_info/cells"))
        .and_then(Value::as_array)
        .map(|cells| cells.len())
        .unwrap_or(0);
    let null_mode = colors_config.get("nullMode").and_then(Value::as_str);
    let nil_value = if null_mode == Some("as-0") {
        Some(0.0)
    } else {
        None
    };
    for (row_index, row) in rows.iter_mut().enumerate() {
        let Some(cells) = row.get_mut("cells").and_then(Value::as_array_mut) else {
            continue;
        };
        let color_values = color_values_by_row
            .get(row_index)
            .cloned()
            .unwrap_or_default();
        for cell_index in row_header_length..cells.len() {
            let color_value = color_values
                .get(cell_index - row_header_length)
                .copied()
                .flatten()
                .or(nil_value);
            let Some(color_value) = color_value else {
                continue;
            };
            let cell = &mut cells[cell_index];
            if cell
                .get("css")
                .and_then(|css| css.get("backgroundColor"))
                .is_some()
            {
                continue;
            }
            let Some(background) = gradient_background(color_value, &colors_config, (min, max))
            else {
                continue;
            };
            let mut css = cell
                .get("css")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            css.insert("backgroundColor".to_string(), json!(background));
            css.insert("color".to_string(), json!("#FFF"));
            css.insert("value".to_string(), json_number(color_value));
            if let Some(map) = cell.as_object_mut() {
                map.insert("css".to_string(), Value::Object(css));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `head` generation (table-head-generator.js)
// ---------------------------------------------------------------------------

struct HeadItem {
    item: Vec<Value>,
    column_sort_meta: Value,
    sub: Vec<HeadItem>,
}

fn map_columns_to_head_cell_data(columns: &[Vec<Value>], columns_meta: &[Value]) -> Vec<HeadItem> {
    fn map_level(
        columns: &[(Vec<Value>, Value)],
        index: usize,
        map: &mut Vec<(String, HeadItem)>,
        parents: &Map<String, Value>,
    ) {
        let mut unique_headers: Vec<String> = Vec::new();
        let mut temp: Vec<(String, Vec<Value>, Value, Vec<bool>)> = Vec::new();
        for (row_index, (column, meta)) in columns.iter().enumerate() {
            let Some(level) = column.get(index) else {
                continue;
            };
            let cell_data = level.as_array().cloned().unwrap_or_default();
            if cell_data.is_empty() {
                continue;
            }
            let cell_value = get_cell_value_for_header(&cell_data[0], None);
            let previous_cell_value = if row_index > 0 {
                columns[row_index - 1]
                    .0
                    .get(index)
                    .and_then(Value::as_array)
                    .and_then(|cells| cells.first())
                    .map(|cell| get_cell_value_for_header(cell, None))
            } else {
                None
            };
            let mut key = format!("_{}__{}", js_string(&cell_value), index);
            let existing = temp
                .iter()
                .position(|(existing_key, _, _, _)| *existing_key == key);
            if existing.is_some() && (column.len() == 1 || previous_cell_value.as_ref() != Some(&cell_value)) {
                key = format!("{key}_{row_index}");
            } else if existing.is_some() && previous_cell_value.as_ref() == Some(&cell_value) {
                if let Some(last) = unique_headers.last() {
                    key = last.clone();
                }
            }
            if let Some(position) = temp
                .iter()
                .position(|(existing_key, _, _, _)| *existing_key == key)
            {
                temp[position].3[row_index] = true;
            } else {
                let mut indexes = vec![false; columns.len()];
                indexes[row_index] = true;
                temp.push((key.clone(), cell_data, meta.clone(), indexes));
            }
            unique_headers.push(key);
        }
        for (key, cell_data, meta, indexes) in temp {
            let cell_value = cell_data.first().cloned().unwrap_or(Value::Null);
            let mut next_parent = parents.clone();
            next_parent.insert(js_string(&cell_value), json!(true));
            let sub_columns: Vec<(Vec<Value>, Value)> = columns
                .iter()
                .zip(indexes.iter())
                .filter(|(_, selected)| **selected)
                .map(|(entry, _)| entry.clone())
                .collect();
            let mut sub: Vec<(String, HeadItem)> = Vec::new();
            if sub_columns.iter().any(|(column, _)| column.len() > index + 1) {
                map_level(&sub_columns, index + 1, &mut sub, &next_parent);
            }
            map.push((
                key,
                HeadItem {
                    item: cell_data,
                    column_sort_meta: meta,
                    sub: sub.into_iter().map(|(_, item)| item).collect(),
                },
            ));
        }
    }

    let owned: Vec<(Vec<Value>, Value)> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            (
                column.clone(),
                columns_meta.get(index).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    let mut map: Vec<(String, HeadItem)> = Vec::new();
    map_level(&owned, 0, &mut map, &Map::new());
    map.into_iter().map(|(_, item)| item).collect()
}

fn cell_type_for_field(field: &Value) -> Option<&'static str> {
    if is_number_field(field) {
        Some("number")
    } else if is_date_field(field) {
        Some("date")
    } else {
        None
    }
}

fn column_width_value(column_settings: Option<&Value>) -> Option<String> {
    let width = column_settings?.get("width")?;
    match width.get("mode").and_then(Value::as_str) {
        Some("percent") => width
            .get("value")
            .map(|value| format!("{}%", js_string(value))),
        Some("pixel") => width
            .get("value")
            .map(|value| format!("{}px", js_string(value))),
        _ => None,
    }
}

fn generated_head_cell(
    ctx: &PivotContext<'_>,
    pivot_data_cell_value: &Value,
    is_total_header: bool,
    measures: &[Value],
    settings_by_field: &HashMap<String, Value>,
) -> Result<Value, String> {
    let Some(components) = pivot_data_cell_value.as_array() else {
        return Ok(json!({}));
    };
    let name = components.first().cloned().unwrap_or(Value::Null);
    let legend_item_id = components.get(1).and_then(Value::as_i64).unwrap_or(-1);
    let pivot_field = ctx
        .fields_by_legend
        .get(&legend_item_id)
        .cloned()
        .unwrap_or_else(|| json!({}));
    let is_measure_name_item =
        pivot_field.get("id").and_then(Value::as_str) == Some(MEASURE_NAME_PSEUDO_ID);
    let field_guid_or_title = if is_measure_name_item {
        js_string(&name).trim_start_matches("title-").to_string()
    } else {
        field_id(&pivot_field).unwrap_or("").to_string()
    };
    let dataset_field = ctx.field_dict.get(&field_guid_or_title).cloned();
    let mut cell = Map::new();
    cell.insert("name".to_string(), name.clone());
    // JS insertion order for a generated column header: name, css, isTotalCell, type, ...
    cell.insert(
        "css".to_string(),
        if is_total_header {
            table_totals_styles()
        } else {
            json!({})
        },
    );
    cell.insert("isTotalCell".to_string(), json!(is_total_header));
    if let Some(measure_item) = measures.first() {
        if let Some(cell_type) = cell_type_for_field(measure_item) {
            cell.insert("type".to_string(), json!(cell_type));
        }
    }
    let Some(dataset_field) = dataset_field else {
        return Ok(Value::Object(cell));
    };
    let column_settings_field_id = if is_measure_name_item {
        pivot_field
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        field_guid_or_title.clone()
    };
    let column_settings = settings_by_field
        .get(&column_settings_field_id)
        .and_then(|settings| settings.get("columnSettings"))
        .and_then(|column_settings| column_settings.get("column"));
    if let Some(width) = column_width_value(column_settings) {
        cell.insert("width".to_string(), json!(width));
    }
    if is_measure_name_item {
        let fake_title = dataset_field
            .get("fakeTitle")
            .and_then(Value::as_str)
            .unwrap_or("");
        cell.insert(
            "name".to_string(),
            json!(if fake_title.is_empty() {
                js_string(&name)
            } else {
                fake_title.to_string()
            }),
        );
        return Ok(Value::Object(cell));
    }
    if name.is_null() {
        cell.insert("formattedName".to_string(), json!(js_string(&name)));
        cell.insert("sortable".to_string(), json!(false));
    } else if is_number_field(&dataset_field) {
        let number = number_value(&name).unwrap_or(0.0);
        cell.insert(
            "formattedName".to_string(),
            json!(chart_kit_format_number(number, &dataset_field)?),
        );
    } else if is_date_field(&dataset_field) {
        cell.insert(
            "name".to_string(),
            json!(format_date_value(
                field_data_type(&dataset_field),
                &name,
                dataset_field.get("format").and_then(Value::as_str),
            )?),
        );
    }
    Ok(Value::Object(cell))
}

fn map_head_data_to_head(
    ctx: &PivotContext<'_>,
    data: &[HeadItem],
    measures: &[Value],
    settings_by_field: &HashMap<String, Value>,
    sort: &HeadSortSettings<'_>,
    is_sort_by_column_allowed: bool,
    is_totals_enabled: bool,
    is_total_sub_header: bool,
    pivot_structure: &Value,
    cell_id: &mut i64,
    path_prefix: &[Value],
) -> Result<Vec<Value>, String> {
    let mut head: Vec<Value> = Vec::with_capacity(data.len());
    let mut measure_guid = sort.measure_guid.to_string();
    for (index, head_item) in data.iter().enumerate() {
        let row = &head_item.item;
        let Some(pivot_data_cell_value) = row.first().cloned() else {
            continue;
        };
        let components = pivot_data_cell_value
            .as_array()
            .cloned()
            .unwrap_or_default();
        let value = components.first().cloned().unwrap_or(Value::Null);
        let legend_item_id = components.get(1).and_then(Value::as_i64).unwrap_or(-1);
        let field = ctx
            .fields_by_legend
            .get(&legend_item_id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let is_measure = field.get("item_type").and_then(Value::as_str) == Some("measure_name");
        let field_guid = if is_measure {
            js_string(&value).trim_start_matches("title-").to_string()
        } else {
            field_id(&field).unwrap_or("").to_string()
        };
        let mut dataset_field = ctx.field_dict.get(&field_guid).cloned();
        // `table-head-generator.ts:492-505`: total cells are placeholders; their field is
        // resolved through the pivot structure item (`legend_item_ids`).
        let is_total_header =
            is_total_sub_header || (is_totals_enabled && is_placeholder_field(&field));
        if is_total_header && dataset_field.is_none() {
            let pivot_item_id = components.get(2).and_then(Value::as_i64).unwrap_or(-1);
            if let Some(item) = pivot_structure.as_array().and_then(|items| {
                items.iter().find(|item| {
                    item.get("pivot_item_id").and_then(Value::as_i64) == Some(pivot_item_id)
                })
            }) {
                for legend in item
                    .get("legend_item_ids")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let resolved = legend
                        .as_i64()
                        .and_then(|legend| ctx.fields_by_legend.get(&legend))
                        .and_then(field_id)
                        .and_then(|guid| ctx.field_dict.get(guid));
                    if let Some(resolved) = resolved {
                        dataset_field = Some(resolved.clone());
                        break;
                    }
                }
            }
        }
        let measures_for_item: Vec<Value> = if is_measure {
            measures.iter().skip(index).cloned().collect()
        } else {
            measures.to_vec()
        };
        let field_order_item = if dataset_field.as_ref().map(is_measure_field).unwrap_or(false) {
            if let Some(dataset_field) = &dataset_field {
                measure_guid = field_id(dataset_field).unwrap_or("").to_string();
            }
            json!("measure_name")
        } else {
            json!(dataset_field.as_ref().and_then(field_id).unwrap_or(""))
        };
        let mut result = Map::new();
        match dataset_field.as_ref().and_then(field_id) {
            Some(guid) => {
                result.insert(
                    "id".to_string(),
                    json!(format!("fieldId={guid}__index={cell_id}")),
                );
            }
            None => {
                result.insert("id".to_string(), json!(format!("fieldId=__index={cell_id}")));
            }
        }
        *cell_id += 1;
        result.insert("sortable".to_string(), json!(true));
        result.insert("name".to_string(), value.clone());
        result.insert("allowGroupSort".to_string(), json!(true));
        if !head_item.sub.is_empty() {
            let nested_sort = HeadSortSettings {
                measure_guid: sort.measure_guid,
                is_sort_by_column_allowed: sort.is_sort_by_column_allowed,
                field_order: sort
                    .field_order
                    .iter()
                    .cloned()
                    .chain(std::iter::once(field_order_item.clone()))
                    .collect(),
            };
            let mut nested_prefix = path_prefix.to_vec();
            nested_prefix.push(value.clone());
            let sub = map_head_data_to_head(
                ctx,
                &head_item.sub,
                &measures_for_item,
                settings_by_field,
                &nested_sort,
                is_sort_by_column_allowed,
                is_totals_enabled,
                is_total_header,
                pivot_structure,
                cell_id,
                &nested_prefix,
            )?;
            result.insert("sub".to_string(), Value::Array(sub));
        } else if !head_item.column_sort_meta.is_null() && is_sort_by_column_allowed {
            let field_order: Vec<Value> = sort
                .field_order
                .iter()
                .cloned()
                .chain(std::iter::once(field_order_item.clone()))
                .collect();
            let mut sort_path = path_prefix.to_vec();
            sort_path.push(value.clone());
            let (sort_meta, _direction) = sort_meta_parts(
                &head_item.column_sort_meta,
                Value::Array(sort_path),
                &measure_guid,
                Value::Array(field_order),
            );
            result.insert("custom".to_string(), sort_meta);
        }
        let metadata = generated_head_cell(
            ctx,
            &pivot_data_cell_value,
            is_total_header,
            &measures_for_item,
            settings_by_field,
        )?;
        if let Some(metadata_map) = metadata.as_object() {
            for (key, value) in metadata_map {
                result.insert(key.clone(), value.clone());
            }
        }
        head.push(Value::Object(result));
    }
    Ok(head)
}

struct HeadSortSettings<'a> {
    measure_guid: &'a str,
    is_sort_by_column_allowed: bool,
    field_order: Vec<Value>,
}

fn generate_table_head(ctx: &PivotContext<'_>) -> Result<Value, String> {
    let pivot_data = ctx
        .response
        .get("pivot_data")
        .ok_or_else(|| "pivot response has no pivot_data".to_string())?;
    let columns_with_info = pivot_data
        .get("columns_with_info")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let columns_data: Vec<Vec<Value>> = columns_with_info
        .iter()
        .map(|column| {
            column
                .get("cells")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|level| Value::Array(level.as_array().cloned().unwrap_or_default()))
                .collect()
        })
        .collect();
    let columns_meta: Vec<Value> = columns_with_info
        .iter()
        .map(|column| column.get("header_info").cloned().unwrap_or(Value::Null))
        .collect();

    let extra = ctx
        .shared
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pivot_structure = ctx
        .response
        .pointer("/pivot/structure")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let pivot_totals = pivot_sub_totals(ctx.shared);
    let is_inline_sort_enabled =
        extra.get("pivotInlineSort").and_then(Value::as_str) != Some("off");
    let is_sort_by_column_allowed =
        is_sort_by_role_allowed(&pivot_structure, &pivot_totals, "pivot_column");

    let measures = placeholder_items(ctx.shared, "measures");
    let settings_by_field = &ctx.field_dict;
    let mapped = map_columns_to_head_cell_data(&columns_data, &columns_meta);

    let table_rows = pivot_data
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let row_headers_length = table_rows
        .first()
        .and_then(|row| row.pointer("/header_with_info/cells"))
        .and_then(Value::as_array)
        .map(|cells| cells.len())
        .unwrap_or(0);
    let line_header_length = if columns_data.len() == 1
        && columns_data.first().map(Vec::is_empty).unwrap_or(true)
    {
        let row_values_length = table_rows
            .first()
            .and_then(|row| row.get("values"))
            .and_then(Value::as_array)
            .map(|values| values.len())
            .unwrap_or(0);
        row_headers_length + row_values_length
    } else {
        row_headers_length
    };
    let _ = is_inline_sort_enabled;

    let sort = HeadSortSettings {
        measure_guid: "",
        is_sort_by_column_allowed,
        field_order: Vec::new(),
    };
    let mut cell_id: i64 = 0;
    let mut head = map_head_data_to_head(
        ctx,
        &mapped,
        &measures,
        settings_by_field,
        &sort,
        is_sort_by_column_allowed,
        is_totals_enabled(ctx.shared),
        false,
        &pivot_structure,
        &mut cell_id,
        &[],
    )?;

    // Prepend the row-dimension placeholder cells (`lineHeaderLength`).
    let use_group = line_header_length > 1;
    let mut head_cell = Map::new();
    head_cell.insert("autogroup".to_string(), json!(false));
    head_cell.insert("group".to_string(), json!(use_group));
    head_cell.insert("name".to_string(), json!(""));
    head_cell.insert("sortable".to_string(), json!(true));
    head_cell.insert("allowGroupSort".to_string(), json!(use_group));
    for _ in 0..line_header_length {
        head.insert(0, Value::Object(head_cell.clone()));
    }

    // Row-dimension headers (`pivotData.row_dimension_headers`).
    let pinned = extra.get("pinnedColumns").and_then(Value::as_i64).unwrap_or(0);
    if let Some(row_dimension_headers) = pivot_data
        .get("row_dimension_headers")
        .and_then(Value::as_array)
    {
        for (index, item) in row_dimension_headers.iter().enumerate() {
            let Some(head_cell) = head.get_mut(index) else {
                continue;
            };
            let Some(first) = item.as_array().and_then(|vector| vector.first()) else {
                continue;
            };
            let mut field_name = first
                .as_array()
                .and_then(|cell| cell.first())
                .map(js_string)
                .unwrap_or_default();
            if let Some(rest) = field_name.strip_prefix("title-") {
                field_name = rest.to_string();
            }
            let Some(field) = ctx.field_dict.get(&field_name).cloned() else {
                continue;
            };
            field_name = fake_title_or_title(&field);
            let rows_items = placeholder_items(ctx.shared, "rows");
            let column_settings = rows_items
                .get(index)
                .and_then(|item| item.get("columnSettings"));
            if let Some(width) = column_width_value(column_settings) {
                head_cell["width"] = json!(width);
            }
            if is_measure_name(&field) {
                head_cell["sortable"] = json!(false);
            }
            let cell_type = cell_type_for_field(&field);
            let field_guid = field_id(&field).map(str::to_string);
            if let Some(guid) = &field_guid {
                head_cell["id"] = json!(format!("fieldId={guid}__index={cell_id}"));
            }
            cell_id += 1;
            // JS insertion order: id, type, fieldId, format, header.
            match cell_type {
                Some(cell_type) => {
                    head_cell["type"] = json!(cell_type);
                }
                None => {
                    if let Some(map) = head_cell.as_object_mut() {
                        map.remove("type");
                    }
                }
            }
            if let Some(guid) = &field_guid {
                head_cell["fieldId"] = json!(guid);
            }
            if cell_type == Some("date") {
                let format = field
                    .get("format")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if matches!(field_data_type(&field), "datetime" | "genericdatetime") {
                            DEFAULT_DATETIME_FORMAT.to_string()
                        } else {
                            DEFAULT_DATE_FORMAT.to_string()
                        }
                    });
                head_cell["format"] = json!(format);
            }
            if field
                .pointer("/hintSettings/enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                if let Some(hint) = field.pointer("/hintSettings/text").cloned() {
                    head_cell["hint"] = hint;
                }
            }
            head_cell["name"] = json!(field_name);
            head_cell["header"] = json!(true);
            if (index as i64) < pinned {
                head_cell["pinned"] = json!(true);
            }
        }
    }

    Ok(Value::Array(head))
}

// ---------------------------------------------------------------------------
// Public data / config / extra / envelope builders
// ---------------------------------------------------------------------------

/// Build the pivot table `data` (`head`, `rows`, `footer`).
pub fn build_data(
    shared: &Value,
    params: &Value,
    response: &Value,
    lang: &str,
) -> Result<Value, String> {
    let Some(pivot_data) = response.get("pivot_data") else {
        return Err("data-api response is not a pivot response".to_string());
    };
    let (expanded, _drill) = preprocess_hierarchies(shared, params);
    let shared = &expanded;
    let rows_count = pivot_data
        .pointer("/rows")
        .and_then(Value::as_array)
        .map(|rows| rows.len())
        .unwrap_or(0);
    let columns_count = pivot_data
        .pointer("/columns_with_info")
        .and_then(Value::as_array)
        .map(|columns| columns.len())
        .unwrap_or(0);
    if rows_count == 0 && columns_count == 0 {
        return Ok(json!({"head": [], "rows": [], "footer": []}));
    }
    let ctx = build_context(shared, response, lang)?;
    let mut rows = generate_table_rows(&ctx)?;
    let mut head = generate_table_head(&ctx)?;
    let mut footer: Vec<Value> = Vec::new();
    if is_totals_enabled(shared) {
        set_totals_headers(&mut rows, &mut head, lang);
        if let Some(total_row_index) = get_grand_totals_row_index(&rows) {
            footer = rows.split_off(total_row_index);
        }
    }
    Ok(json!({
        "head": head,
        "rows": rows,
        "footer": footer,
    }))
}

/// Build the serialized `config` member for a pivot table.
pub fn build_config(shared: &Value, params: &Value, rows_length: usize) -> Result<String, String> {
    let extra = shared
        .get("extraSettings")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut config = Map::new();
    if let Some(title) = shared.get("title").and_then(Value::as_str) {
        config.insert("title".to_string(), json!(title));
    }
    config.insert("hideHolidaysBands".to_string(), json!(true));
    config.insert("linesLimit".to_string(), json!(DEFAULT_CHART_LINES_LIMIT));
    config.insert(
        "tooltip".to_string(),
        json!({"pin": {"altKey": true}, "sort": {"enabled": true}}),
    );
    config.insert("preventDefaultForPointClick".to_string(), json!(false));
    if shared.get("extraSettings").is_some() {
        if extra.get("title").and_then(Value::as_str).is_some()
            && extra.get("titleMode").and_then(Value::as_str) == Some("show")
        {
            config.insert(
                "title".to_string(),
                json!(extra.get("title").and_then(Value::as_str).unwrap_or("")),
            );
        }
        let items = all_placeholder_items(shared);
        let has_dimensions = items.iter().any(|item| {
            is_dimension_field(item)
                || item.get("data_type").and_then(Value::as_str) == Some("hierarchy")
        });
        let limit = extra.get("limit").cloned().filter(js_truthy);
        let pagination_on =
            has_dimensions && extra.get("pagination").and_then(Value::as_str) == Some("on");
        let mut paginator = Map::new();
        paginator.insert("enabled".to_string(), json!(pagination_on));
        if let Some(limit) = &limit {
            paginator.insert("limit".to_string(), limit.clone());
        }
        config.insert("paginator".to_string(), Value::Object(paginator));
        let mut matched_params: Vec<Value> = Vec::new();
        if let Some(partials) = shared.get("datasetsPartialFields").and_then(Value::as_array) {
            for fields in partials {
                if let Some(fields) = fields.as_array() {
                    for field in fields {
                        if let Some(title) = field.get("title").and_then(Value::as_str) {
                            if params.get(title).is_some() {
                                matched_params.push(json!(title));
                            }
                        }
                    }
                }
            }
        }
        let mut comments = Map::new();
        comments.insert("matchType".to_string(), json!("intersection"));
        comments.insert("matchedParams".to_string(), Value::Array(matched_params.clone()));
        if let Some(feed) = extra.get("feed").cloned().filter(js_truthy) {
            comments.insert(
                "feeds".to_string(),
                json!([{
                    "feed": feed,
                    "matchedParams": matched_params,
                    "matchType": "intersection",
                }]),
            );
        }
        config.insert("comments".to_string(), Value::Object(comments));
        if let Some(size) = extra.get("size").cloned().filter(js_truthy) {
            config.insert("size".to_string(), size);
        }
        if extra
            .get("enableGPTInsights")
            .and_then(Value::as_bool)
            .is_some()
        {
            config.insert(
                "enableGPTInsights".to_string(),
                extra.get("enableGPTInsights").cloned().unwrap_or(Value::Null),
            );
        }
    }
    if shared.pointer("/visualization/id").and_then(Value::as_str) == Some("pivotTable") {
        config.insert(
            "settings".to_string(),
            json!({"externalSort": true, "width": "max-content"}),
        );
    }
    // `js.ts:457-468`: a chart with a hierarchy gets the drill-down breadcrumbs and the
    // server date format of the current level (the UI formats the drilled value with it).
    if let (_, Some(drill)) = preprocess_hierarchies(shared, params) {
        let current_type = drill
            .fields
            .get(drill.level)
            .map(field_data_type)
            .unwrap_or("");
        let date_format = if matches!(current_type, "datetime" | "genericdatetime") {
            "YYYY-MM-DDTHH:mm:ss"
        } else {
            "YYYY-MM-DD"
        };
        config.insert(
            "drillDown".to_string(),
            json!({"breadcrumbs": drill.breadcrumbs, "dateFormat": date_format}),
        );
    }
    if extra
        .get("preserveWhiteSpace")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        config.insert("preserveWhiteSpace".to_string(), json!(true));
    }
    // `buildGraphPrivate` disables the paginator when the first page is short.
    let page = get_current_page(params);
    if let Some(limit) = extra.get("limit").and_then(number_value) {
        if page == 1.0 && limit > rows_length as f64 {
            if let Some(paginator) = config
                .get_mut("paginator")
                .and_then(Value::as_object_mut)
            {
                paginator.insert("enabled".to_string(), json!(false));
            }
        }
    }
    Ok(serde_json::to_string(&Value::Object(config)).unwrap_or_else(|_| "{}".to_string()))
}

/// Build the `extra` member (`{datasets: [...]}`) for a pivot table.
/// `ChartEditor.setExtra('datasets', datasetsMeta)` as `js/js.ts` builds it (apiVersion 2).
///
/// `datasets` is the response-field view of every source in the request:
/// `[{"id": <datasetId>, "fields": <data-api result fields>}, ...]`.
///
/// * `fields` map: every saved-schema item (`datasetsPartialFields[datasetIndex]`) whose guid
///   has a title in the merged fields (`fields[item.guid] = idToTitle[...]`; a missing title is
///   omitted, matching `JSON.stringify` dropping `undefined` keys);
/// * `fieldsList`: `getFieldList(mergedFields, visualization.placeholders)` — the merged
///   response fields in their own order, `title`/`dataType` from the field, and `formatting`
///   only when the field is used by a visualization placeholder (then
///   `getFormatOptions({...field, ...placeholdersField})`); `fakeTitle` is carried through.
pub fn build_extra(shared: &Value, datasets: &Value) -> Value {
    let placeholders_items: Vec<&Value> = all_placeholder_items(shared);
    let mut id_to_title: HashMap<&str, &str> = HashMap::new();
    for dataset in datasets.as_array().into_iter().flatten() {
        for field in dataset
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(guid) = field_id(field) else {
                continue;
            };
            if let Some(title) = field.get("title").and_then(Value::as_str) {
                id_to_title.insert(guid, title);
            }
        }
    }
    let saved_schemas = shared
        .get("datasetsPartialFields")
        .and_then(Value::as_array);
    let mut datasets_meta: Vec<Value> = Vec::new();
    let mut seen_datasets: Vec<&str> = Vec::new();
    for dataset in datasets.as_array().into_iter().flatten() {
        let Some(dataset_id) = dataset.get("id").and_then(Value::as_str) else {
            continue;
        };
        // Reference `datasetsMeta` is indexed by datasetIndex (`if (acc[i]) return acc`):
        // a combined chart with several layers of one dataset yields one entry, not one per layer.
        if seen_datasets.contains(&dataset_id) {
            continue;
        }
        seen_datasets.push(dataset_id);
        let Some(response_fields) = dataset.get("fields").and_then(Value::as_array) else {
            continue;
        };
        let schema_index = shared
            .get("datasetsIds")
            .and_then(Value::as_array)
            .and_then(|ids| ids.iter().position(|id| id.as_str() == Some(dataset_id)))
            .unwrap_or(0);
        let mut fields_map = Map::new();
        if let Some(schema) = saved_schemas
            .and_then(|schemas| schemas.get(schema_index))
            .and_then(Value::as_array)
        {
            for item in schema {
                let Some(guid) = item.get("guid").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(title) = id_to_title.get(guid) {
                    fields_map.insert(guid.to_string(), json!(title));
                }
            }
        }
        let mut fields_list: Vec<Value> = Vec::new();
        for field in response_fields {
            let Some(guid) = field_id(field) else {
                continue;
            };
            let mut entry = Map::new();
            entry.insert(
                "title".to_string(),
                field.get("title").cloned().unwrap_or(Value::Null),
            );
            entry.insert("guid".to_string(), json!(guid));
            entry.insert(
                "dataType".to_string(),
                field.get("data_type").cloned().unwrap_or(Value::Null),
            );
            if let Some(placeholders_field) = placeholders_items
                .iter()
                .find(|placeholder| field_id(placeholder) == Some(guid))
            {
                let mut merged = field.clone();
                if let (Some(target), Some(source)) =
                    (merged.as_object_mut(), placeholders_field.as_object())
                {
                    for (key, value) in source {
                        target.insert(key.clone(), value.clone());
                    }
                }
                // JS `getFormatOptions` returns `undefined` for non-numeric fields; a missing
                // `formatting` key is what the wire contract sees (JSON drops undefined).
                if let Some(formatting) = get_format_options(&merged) {
                    entry.insert("formatting".to_string(), formatting);
                }
            }
            fields_list.push(Value::Object(entry));
        }
        datasets_meta.push(json!({
            "id": dataset_id,
            "fields": Value::Object(fields_map),
            "fieldsList": Value::Array(fields_list),
        }));
    }
    json!({"datasets": datasets_meta})
}

fn json_object_with_entries(entries: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

/// Inputs for the `/api/run` response envelope.
pub struct EnvelopeInput<'a> {
    pub params: &'a Value,
    pub dataset: &'a str,
    pub request_body: &'a Value,
    pub response: &'a Value,
    pub data: Value,
    pub config: String,
    pub extra: Value,
    pub status: u16,
    pub latency_ms: u64,
    pub js_execution_ms: f64,
    pub size: u64,
    pub entry_id: Value,
    pub key: Value,
    pub rev_id: Value,
    pub response_id: &'a str,
    pub elapsed_ms: f64,
}

/// Build the final `/api/run` envelope with `jsExecution: 0`.
pub fn build_envelope(input: EnvelopeInput<'_>) -> Result<Value, String> {
    let EnvelopeInput {
        params,
        dataset,
        request_body,
        response,
        data,
        config,
        extra,
        status,
        latency_ms,
        js_execution_ms,
        size,
        entry_id,
        key,
        rev_id,
        response_id,
        elapsed_ms,
    } = input;
    let source_key = format!("{dataset}_result");
    let query = response
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("query").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();
    let mut source = Map::new();
    source.insert("sourceId".to_string(), json!(source_key));
    source.insert("sourceType".to_string(), json!("bi_datasets"));
    source.insert("status".to_string(), json!(status));
    source.insert("latency".to_string(), json!(latency_ms));
    source.insert("size".to_string(), json!(size));
    source.insert("datasetId".to_string(), json!(dataset));
    source.insert("data".to_string(), request_body.clone());
    if !query.is_empty() {
        source.insert("info".to_string(), json!({"query": query}));
    }
    let sources = json!({ source_key.clone(): Value::Object(source) });

    // `buildGraphPrivate`: `newParams[fieldId] = []` for every merged field,
    // then `ChartEditor.updateParams` -> `userParamsOverride`.
    let mut overrides = Map::new();
    if let Some(fields) = response.get("fields").and_then(Value::as_array) {
        for field in fields {
            if let Some(guid) = field.get("id").and_then(Value::as_str) {
                overrides.insert(guid.to_string(), json!([]));
            }
            if let Some(guid) = field.get("guid").and_then(Value::as_str) {
                overrides
                    .entry(guid.to_string())
                    .or_insert_with(|| json!([]));
            }
        }
    }
    let mut final_params = params.clone();
    let mut used_params = Map::new();
    if let Some(final_map) = final_params.as_object_mut() {
        for (param, override_value) in &overrides {
            let current = final_map.get(param).cloned().unwrap_or(Value::Null);
            let current_has_value = match &current {
                Value::Array(items) => !items.is_empty(),
                Value::String(text) => !text.is_empty(),
                Value::Null => false,
                _ => true,
            };
            let override_has_value = override_value
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false);
            if current_has_value {
                if override_has_value {
                    final_map.insert(param.clone(), override_value.clone());
                }
            } else {
                final_map.insert(param.clone(), override_value.clone());
            }
            used_params.insert(
                param.clone(),
                final_map.get(param).cloned().unwrap_or(Value::Null),
            );
        }
    }

    let data_export = json!({
        source_key.clone(): response.get("data_export").cloned().unwrap_or(Value::Null),
    });
    let config_resolving =
        (elapsed_ms - latency_ms as f64 - js_execution_ms).max(0.0);
    let conf = json!({
        "type": "table_wizard_node",
        "meta": {"stype": "table_wizard_node"},
        "entryId": entry_id,
        "key": key,
        "revId": rev_id,
    });
    let _ = config.as_str();
    Ok(json_object_with_entries(vec![
        ("sources", sources),
        ("uiScheme", Value::Null),
        ("params", final_params),
        ("usedParams", Value::Object(used_params)),
        ("actionParams", json!({})),
        ("widgetConfig", json!({})),
        ("defaultParams", json!({})),
        ("extra", extra),
        (
            "timings",
            json!({
                "configResolving": config_resolving,
                "dataFetching": latency_ms as f64,
                "jsExecution": js_execution_ms,
            }),
        ),
        ("data", data),
        ("config", json!(config)),
        ("highchartsConfig", json!("{}")),
        ("dataExport", data_export),
        ("_confStorageConfig", conf),
        ("key", key),
        ("id", json!(response_id)),
        ("type", json!("table_wizard_node")),
        ("revId", rev_id),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_two_point_endpoints() {
        let config = json!({"gradientMode": "2-point", "gradientPalette": "blue"});
        assert_eq!(
            gradient_background(0.0, &config, (0.0, 10.0)).as_deref(),
            Some("rgb(0, 68, 163)")
        );
        assert_eq!(
            gradient_background(10.0, &config, (0.0, 10.0)).as_deref(),
            Some("rgb(140, 203, 255)")
        );
    }

    #[test]
    fn gradient_three_point_endpoints() {
        let config = json!({
            "gradientMode": "3-point",
            "gradientPalette": "yellow-green-blue",
            "thresholdsMode": "auto"
        });
        assert_eq!(
            gradient_background(0.0, &config, (0.0, 1.0)).as_deref(),
            Some("rgb(255, 197, 51)")
        );
        assert_eq!(
            gradient_background(1.0, &config, (0.0, 1.0)).as_deref(),
            Some("rgb(105, 182, 255)")
        );
    }

    #[test]
    fn number_format_precision_zero() {
        let field = json!({
            "data_type": "float",
            "formatting": {
                "format": "number",
                "showRankDelimiter": true,
                "prefix": "",
                "postfix": "",
                "units": null,
                "precision": 0
            }
        });
        assert_eq!(chart_kit_format_number(0.0, &field).unwrap(), "0");
        assert_eq!(chart_kit_format_number(10.0, &field).unwrap(), "10");
    }

    #[test]
    fn date_format_default() {
        let value = json!("2019-05-01");
        assert_eq!(
            format_date_value("date", &value, Some("DD.MM.YYYY")).unwrap(),
            "01.05.2019"
        );
    }

    #[test]
    fn sort_direction_cycle() {
        assert_eq!(get_sort_direction(None), Some("desc"));
        assert_eq!(get_sort_direction(Some("desc")), Some("asc"));
        assert_eq!(get_sort_direction(Some("asc")), None);
    }

    #[test]
    fn resolve_interval_params_is_identity_for_absolute_values() {
        let params = json!({
            "p": ["__interval_2019-04-01T00:00:00.000Z_2020-02-29T00:00:00.000Z"]
        });
        let resolved = resolve_params(&params).unwrap();
        assert_eq!(resolved, params);
    }

    #[test]
    fn filter_values_absolute_interval_becomes_between_pair() {
        // A saved chart filter keeps the dashboard date range as one string; the data-api
        // `BETWEEN` needs the two resolved values (this is the CH `accrual_date_ltov` 400).
        let values = vec![json!("__interval_2026-08-29T00:00:00.000Z_2026-09-11T23:59:59.999Z")];
        let (resolved, operation) = resolve_filter_values(&values, "BETWEEN");
        assert_eq!(operation, "BETWEEN");
        assert_eq!(
            resolved,
            vec![json!("2026-08-29T00:00:00.000Z"), json!("2026-09-11T23:59:59.999Z")]
        );
    }

    #[test]
    fn filter_values_relative_interval_sides_are_resolved_as_start_and_end() {
        // `__interval___relative_-180d___relative_-2d`: the from side is the start of the
        // day `-180d` ago, the to side the end of the day `-2d` ago (both relative markers).
        let values = vec![json!("__interval___relative_-180d___relative_-2d")];
        let (resolved, operation) = resolve_filter_values(&values, "BETWEEN");
        assert_eq!(operation, "BETWEEN");
        assert_eq!(resolved.len(), 2);
        let from = resolved[0].as_str().expect("from is a string");
        let to = resolved[1].as_str().expect("to is a string");
        assert!(from.ends_with("T00:00:00.000Z"), "from = {from}");
        assert!(to.ends_with("T23:59:59.999Z"), "to = {to}");
        assert!(from < to, "from {from} must precede to {to}");
        let from_date = parse_date_value(from).expect("from parses");
        let to_date = parse_date_value(to).expect("to parses");
        assert_eq!((to_date - from_date).num_days(), 178);
    }

    #[test]
    fn filter_values_absolute_from_with_relative_to() {
        // `__interval_2024-02-01T00:00:00.000Z___relative_-10d`: one absolute, one relative.
        let values = vec![json!("__interval_2024-02-01T00:00:00.000Z___relative_-10d")];
        let (resolved, operation) = resolve_filter_values(&values, "BETWEEN");
        assert_eq!(operation, "BETWEEN");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0], json!("2024-02-01T00:00:00.000Z"));
        let to = resolved[1].as_str().expect("to is a string");
        assert!(to.ends_with("T23:59:59.999Z"), "to = {to}");
    }

    #[test]
    fn filter_values_lone_relative_uses_operation_endpoint() {
        let values = vec![json!("__relative_-7d")];
        let (start, operation) = resolve_filter_values(&values, "GTE");
        assert_eq!(operation, "GTE");
        assert!(start[0].as_str().unwrap().ends_with("T00:00:00.000Z"));
        let (end, _) = resolve_filter_values(&values, "LTE");
        assert!(end[0].as_str().unwrap().ends_with("T23:59:59.999Z"));
    }

    #[test]
    fn filter_values_plain_values_are_kept() {
        let values = vec![json!("Доставка покупателю"), json!("Самовывоз")];
        let (resolved, operation) = resolve_filter_values(&values, "IN");
        assert_eq!(resolved, values);
        assert_eq!(operation, "IN");
        let single = vec![json!("1049407186")];
        let (resolved, operation) = resolve_filter_values(&single, "IN");
        assert_eq!(resolved, single);
        assert_eq!(operation, "IN");
    }

    #[test]
    fn filter_values_malformed_interval_is_forwarded_unchanged() {
        // `resolveIntervalDate` returns null for a malformed marker and the JS engine then
        // forwards the raw string (the data-api reports `INVALID_VALUE`); it must not be
        // silently turned into a different filter.
        let values = vec![json!("__interval___relative_")];
        let (resolved, operation) = resolve_filter_values(&values, "BETWEEN");
        assert_eq!(resolved, values);
        assert_eq!(operation, "BETWEEN");
    }

    #[test]
    fn config_for_pivot_is_stable() {
        let shared = json!({
            "visualization": {
                "id": "pivotTable",
                "placeholders": [
                    {"id": "pivot-table-columns", "items": [{"guid": "a", "data_type": "float", "type": "DIMENSION"}]},
                    {"id": "rows", "items": [{"guid": "b", "data_type": "date", "type": "DIMENSION"}]},
                    {"id": "measures", "items": [{"guid": "c", "data_type": "string", "type": "MEASURE"}]}
                ]
            },
            "extraSettings": {},
            "datasetsPartialFields": [[]]
        });
        let config: Value =
            serde_json::from_str(&build_config(&shared, &json!({}), 10).unwrap()).unwrap();
        assert_eq!(
            config,
            json!({
                "hideHolidaysBands": true,
                "linesLimit": 100,
                "tooltip": {"pin": {"altKey": true}, "sort": {"enabled": true}},
                "preventDefaultForPointClick": false,
                "paginator": {"enabled": false},
                "comments": {"matchType": "intersection", "matchedParams": []},
                "settings": {"externalSort": true, "width": "max-content"}
            })
        );
    }
}

#[cfg(test)]
mod pivot_forms_tests {
    use super::*;

    /// Shared config of a pivot table: `Measure Names` + `scale` in columns, one row dimension
    /// with sub-totals and two measures. Mirrors `evidence/` of `fixes/pivot/REPORT.md`.
    fn totals_shared() -> Value {
        json!({
            "visualization": {
                "id": "pivotTable",
                "placeholders": [
                    {"id": "pivot-table-columns", "items": [
                        {"type": "PSEUDO", "title": "Measure Names", "data_type": "string"},
                        {"guid": "scale", "title": "scale", "type": "DIMENSION", "data_type": "string",
                         "subTotalsSettings": {"enabled": true}}
                    ]},
                    {"id": "rows", "items": [
                        {"guid": "metric", "title": "metric", "type": "DIMENSION", "data_type": "string",
                         "subTotalsSettings": {"enabled": true}}
                    ]},
                    {"id": "measures", "items": [
                        {"guid": "m1", "title": "Orders", "type": "MEASURE", "data_type": "integer"},
                        {"guid": "m2", "title": "Sales", "type": "MEASURE", "data_type": "float"}
                    ]}
                ]
            },
            "datasetsIds": ["ds"],
            "colors": [],
            "extraSettings": {},
            "datasetsPartialFields": [[]]
        })
    }

    /// A real `pivot_data` shape: 2 column groups (measure names), a totals column per group, a
    /// base row and a grand-total row (`item_type: "placeholder"` + `role_spec.role: "total"`).
    fn totals_response() -> Value {
        json!({
            "fields": [
                {"legend_item_id": 0, "id": "__@measure_name__", "title": "Measure Names",
                 "data_type": "string", "item_type": "measure_name"},
                {"legend_item_id": 1, "id": "scale", "title": "scale",
                 "data_type": "string", "item_type": "field"},
                {"legend_item_id": 2, "id": "metric", "title": "metric",
                 "data_type": "string", "item_type": "field"},
                {"legend_item_id": 3, "id": "m1", "title": "Orders",
                 "data_type": "integer", "item_type": "field"},
                {"legend_item_id": 4, "id": "m2", "title": "Sales",
                 "data_type": "float", "item_type": "field"},
                {"legend_item_id": 6, "id": "__@placeholder__", "title": "Placeholder",
                 "data_type": "string", "item_type": "placeholder"},
                {"legend_item_id": 7, "id": "m1", "title": "Orders",
                 "data_type": "integer", "item_type": "field", "role_spec": {"role": "total"}},
                {"legend_item_id": 8, "id": "m2", "title": "Sales",
                 "data_type": "float", "item_type": "field", "role_spec": {"role": "total"}}
            ],
            "pivot": {"structure": [
                {"pivot_item_id": 0, "legend_item_ids": [0],
                 "role_spec": {"role": "pivot_column", "direction": "asc"}, "title": "Measure Names"},
                {"pivot_item_id": 1, "legend_item_ids": [1],
                 "role_spec": {"role": "pivot_column", "direction": "asc"}, "title": "scale"},
                {"pivot_item_id": 2, "legend_item_ids": [2],
                 "role_spec": {"role": "pivot_row", "direction": "asc"}, "title": "metric"},
                {"pivot_item_id": 3, "legend_item_ids": [3],
                 "role_spec": {"role": "pivot_measure"}, "title": "Orders"},
                {"pivot_item_id": 4, "legend_item_ids": [4],
                 "role_spec": {"role": "pivot_measure"}, "title": "Sales"}
            ]},
            "pivot_data": {
                "columns": [
                    [[["Orders", 0, 0]], [["month", 1, 1]]],
                    [[["Orders", 0, 0]], [["", 6, 1]]],
                    [[["Sales", 0, 0]], [["month", 1, 1]]],
                    [[["Sales", 0, 0]], [["", 6, 1]]]
                ],
                "columns_with_info": [
                    {"cells": [[["Orders", 0, 0]], [["month", 1, 1]]],
                     "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                    {"cells": [[["Orders", 0, 0]], [["", 6, 1]]],
                     "header_info": {"sorting_direction": null, "role_spec": {"role": "total"}}},
                    {"cells": [[["Sales", 0, 0]], [["month", 1, 1]]],
                     "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                    {"cells": [[["Sales", 0, 0]], [["", 6, 1]]],
                     "header_info": {"sorting_direction": null, "role_spec": {"role": "total"}}}
                ],
                "row_dimension_headers": [[["metric", 2, 2]]],
                "rows": [
                    {"header_with_info": {
                        "cells": [[["sales", 2, 2]]],
                        "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                     "values": [[["13326", 3, 3]], [["13326", 7, 3]],
                                [["30697609.8", 4, 4]], [["30697609.8", 8, 4]]]},
                    {"header_with_info": {
                        "cells": [[["", 6, 2]]],
                        "header_info": {"sorting_direction": null, "role_spec": {"role": "template"}}},
                     "values": [[["13326", 7, 3]], [["13326", 7, 3]],
                                [["30697609.8", 8, 4]], [["30697609.8", 8, 4]]]}
                ]
            }
        })
    }

    #[test]
    fn subtotals_are_levels_of_enabled_placeholders() {
        assert_eq!(
            pivot_sub_totals(&totals_shared()),
            json!({"columns": [{"level": 1}], "rows": [{"level": 0}]})
        );
        assert!(pivot_sub_totals(&json!({})).pointer("/rows").is_some());
    }

    #[test]
    fn build_request_sends_measure_names_and_totals() {
        let shared = totals_shared();
        let fields = json!({"revision_id": "rev", "fields": []});
        let request = build_request(&shared, &json!({}), "ds", &json!("rev"), &fields).unwrap();
        let fields = request.get("fields").and_then(Value::as_array).unwrap();
        assert_eq!(fields[0]["ref"], json!({"type": "measure_name"}));
        assert_eq!(fields.len(), 5, "columns, rows and both measures: {fields:?}");
        let totals = request.pointer("/pivot/totals").unwrap();
        assert_eq!(totals, &json!({"columns": [{"level": 1}], "rows": [{"level": 0}]}));
        let roles: Vec<&str> = request
            .pointer("/pivot/structure")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|item| item.pointer("/role_spec/role").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(
            roles,
            ["pivot_column", "pivot_column", "pivot_row", "pivot_measure", "pivot_measure"]
        );
    }

    #[test]
    fn grand_totals_go_to_the_footer_and_are_labelled() {
        let data = build_data(&totals_shared(), &json!({}), &totals_response(), "ru").unwrap();
        let rows = data.get("rows").and_then(Value::as_array).unwrap();
        let footer = data.get("footer").and_then(Value::as_array).unwrap();
        assert_eq!(rows.len(), 1, "the grand-total row is moved to the footer: {data}");
        assert_eq!(footer.len(), 1);
        assert_eq!(footer[0]["cells"][0]["value"], json!("Итого"));
        assert_eq!(footer[0]["cells"][0]["isTotalCell"], json!(true));
        assert_eq!(footer[0]["cells"][0]["type"], json!("text"));
        // The intersection cells of a totals row are total cells too.
        assert_eq!(footer[0]["cells"][2]["isTotalCell"], json!(true));

        let head = data.get("head").and_then(Value::as_array).unwrap();
        // `lineHeaderLength` = 1 row dimension + 2 levels of columns = head[0] + two groups.
        assert_eq!(head[0]["header"], json!(true));
        let measure_names: Vec<&str> = head
            .iter()
            .filter_map(|cell| cell.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(measure_names, ["metric", "Orders", "Sales"]);
        for (group, measure_name) in head[1..].iter().zip(["Orders", "Sales"]) {
            let sub = group.get("sub").and_then(Value::as_array).unwrap();
            assert_eq!(sub[0]["name"], json!("month"));
            assert_eq!(sub[1]["name"], json!(format!("Итого {measure_name}")));
            assert_eq!(sub[1]["isTotalCell"], json!(true));
            assert_eq!(
                sub[1]["css"],
                json!({"background-color": TABLE_TOTALS_BG, "font-weight": 500})
            );
        }
    }

    #[test]
    fn totals_labels_follow_the_chart_language() {
        let data = build_data(&totals_shared(), &json!({}), &totals_response(), "en").unwrap();
        let footer = data.get("footer").and_then(Value::as_array).unwrap();
        assert_eq!(footer[0]["cells"][0]["value"], json!("Total"), "{data}");
        let head = data.get("head").and_then(Value::as_array).unwrap();
        let totals: Vec<&str> = head[1..]
            .iter()
            .flat_map(|group| group.get("sub").and_then(Value::as_array).unwrap())
            .filter(|cell| cell.get("isTotalCell").and_then(Value::as_bool) == Some(true))
            .filter_map(|cell| cell.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(totals, ["Total Orders", "Total Sales"], "{head:?}");
    }

    #[test]
    fn subtotal_cell_is_labelled_with_its_parent_value() {
        let mut rows = vec![json!({"cells": [
            {"value": "sales", "isTotalCell": false},
            {"value": "", "isTotalCell": true, "css": table_totals_styles()}
        ]})];
        let mut head = json!([{"name": "", "sub": [{"name": "Orders"},
            {"name": "Orders", "sub": [{"name": "month"}, {"name": "", "isTotalCell": true}]}]}]);
        set_totals_headers(&mut rows, &mut head, "ru");
        assert_eq!(rows[0]["cells"][1]["value"], json!("Итого sales"));
        assert_eq!(rows[0]["cells"][1]["type"], json!("text"));
        // An empty parent name is falsy in JS, so the label stays "Итого".
        assert_eq!(head[0]["sub"][1]["name"], json!("Orders"));
        assert_eq!(head[0]["sub"][1]["sub"][1]["name"], json!("Итого Orders"));
    }

    #[test]
    fn supports_measure_names_multi_measure_and_sub_totals() {
        assert_eq!(supports(&totals_shared()), Ok(()));
        // `Measure Names` alone in the columns is the same shape.
        let mut measure_names_only = totals_shared();
        measure_names_only["visualization"]["placeholders"][0]["items"] =
            json!([{"type": "PSEUDO", "title": "Measure Names", "data_type": "string"}]);
        assert_eq!(supports(&measure_names_only), Ok(()));
    }

    #[test]
    fn supports_bars_and_background_settings() {
        let mut bars = totals_shared();
        bars["visualization"]["placeholders"][2]["items"][0]["barsSettings"] = json!({"enabled": true});
        assert_eq!(supports(&bars), Ok(()));
        let mut background = totals_shared();
        background["visualization"]["placeholders"][2]["items"][0]["backgroundSettings"] =
            json!({"enabled": true});
        assert_eq!(supports(&background), Ok(()));
    }

    #[test]
    #[test]
    fn supports_ignores_layers_and_links_of_a_single_dataset() {
        let mut shared = totals_shared();
        shared["visualization"]["layers"] = json!([]);
        shared["links"] = json!([{"fields": {"ds": {"field": {"guid": "scale"}}}}]);
        assert_eq!(supports(&shared), Ok(()));
        // A layer list with an actual layer is a graph shape the pivot builder cannot use.
        shared["visualization"]["layers"] = json!([{"id": "layer-1"}]);
        assert_eq!(
            supports(&shared),
            Err("layered pivot tables are not ported".to_string())
        );
        shared["visualization"]["layers"] = json!([]);
        // A second dataset is the mergeData gap, not a layer/links problem.
        shared["datasetsIds"] = json!(["ds", "ds2"]);
        assert_eq!(
            supports(&shared),
            Err("only a single-dataset pivot is ported".to_string())
        );
    }

    fn supports_still_rejects_unported_shapes() {
        let mut fallback = totals_shared();
        fallback["extraSettings"] = json!({"pivotFallback": "on"});
        assert_eq!(supports(&fallback), Err("pivotFallback is on".to_string()));
        let mut other_pseudo = totals_shared();
        other_pseudo["visualization"]["placeholders"][0]["items"][0] =
            json!({"type": "PSEUDO", "title": "Dimension Names", "data_type": "string"});
        assert!(supports(&other_pseudo).is_err());
    }

    #[test]
    fn grand_totals_row_index_is_the_trailing_total_block() {
        let base = json!({"cells": [{"value": "a", "isTotalCell": false}]});
        let total = json!({"cells": [{"value": "", "isTotalCell": true}]});
        assert_eq!(get_grand_totals_row_index(&[base.clone()]), None);
        assert_eq!(get_grand_totals_row_index(&[base.clone(), total.clone()]), Some(1));
        assert_eq!(
            get_grand_totals_row_index(&[total.clone(), base.clone(), total.clone(), total]),
            Some(2)
        );
    }

    fn drill_shared() -> Value {
        json!({
            "visualization": {
                "id": "pivotTable",
                "placeholders": [
                    {"id": "pivot-table-columns", "items": [
                        {"guid": "cat-hierarchy", "title": "Category", "type": "DIMENSION",
                         "data_type": "hierarchy", "fields": [
                            {"guid": "cat1", "title": "Level 1", "type": "DIMENSION", "data_type": "string"},
                            {"guid": "cat2", "title": "Level 2", "type": "DIMENSION", "data_type": "date"}
                         ]}
                    ]},
                    {"id": "rows", "items": [
                        {"guid": "metric", "title": "metric", "type": "DIMENSION", "data_type": "string"}
                    ]},
                    {"id": "measures", "items": [
                        {"guid": "m1", "title": "Orders", "type": "MEASURE", "data_type": "integer"}
                    ]}
                ]
            },
            "datasetsIds": ["ds"],
            "colors": [],
            "extraSettings": {},
            "datasetsPartialFields": [[]]
        })
    }

    fn drill_schema() -> Value {
        json!([
            {"guid": "cat1", "title": "Level 1", "type": "DIMENSION", "data_type": "string"},
            {"guid": "cat2", "title": "Level 2", "type": "DIMENSION", "data_type": "date"},
            {"guid": "metric", "title": "metric", "type": "DIMENSION", "data_type": "string"},
            {"guid": "m1", "title": "Orders", "type": "MEASURE", "data_type": "integer"}
        ])
    }

    #[test]
    fn hierarchy_is_replaced_by_the_current_drill_level_field() {
        let shared = drill_shared();
        assert_eq!(supports(&shared), Ok(()));
        let params = json!({"drillDownLevel": ["1"], "drillDownFilters": ["A", ""]});
        let request = build_request(&shared, &params, "ds", &json!("rev"), &drill_schema()).unwrap();
        let ids: Vec<&str> = request["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|field| field.pointer("/ref/id").and_then(Value::as_str))
            .collect();
        // Level 1 is `cat2`; the hierarchy guid never reaches the data-api.
        assert!(ids.contains(&"cat2"), "{ids:?}");
        assert!(!ids.contains(&"cat1"), "{ids:?}");
        assert!(!ids.contains(&"cat-hierarchy"), "{ids:?}");
        // `drillDownFilters[0]` becomes an `EQ` filter for the first level. User params are
        // forwarded as filters too (the request carries `ignore_nonexistent_filters`), so look
        // the drill filter up instead of comparing the whole list.
        let drill_filter = request["filters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|filter| filter.get("isDrillDown").and_then(Value::as_bool) == Some(true))
            .expect("drill-down filter");
        assert_eq!(
            drill_filter,
            &json!({
                "ref": {"type": "id", "id": "cat1"},
                "operation": "IN",
                "values": ["A"],
                "isDrillDown": true,
            })
        );
        let config: Value =
            serde_json::from_str(&build_config(&shared, &params, 10).unwrap()).unwrap();
        assert_eq!(
            config["drillDown"],
            json!({"breadcrumbs": ["Level 1", "Level 2"], "dateFormat": "YYYY-MM-DD"}),
            "{config}"
        );
    }

    #[test]
    fn hierarchy_at_the_root_level_keeps_the_first_field_and_no_drill_filters() {
        let shared = drill_shared();
        let request = build_request(&shared, &json!({}), "ds", &json!("rev"), &drill_schema()).unwrap();
        let ids: Vec<&str> = request["fields"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|field| field.pointer("/ref/id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"cat1"), "{ids:?}");
        assert!(request["filters"].as_array().unwrap().is_empty());
        let config: Value =
            serde_json::from_str(&build_config(&shared, &json!({}), 10).unwrap()).unwrap();
        assert_eq!(
            config["drillDown"],
            json!({"breadcrumbs": ["Level 1", "Level 2"], "dateFormat": "YYYY-MM-DD"})
        );
    }

    fn settings_shared(measure_settings: Value) -> Value {
        json!({
            "visualization": {
                "id": "pivotTable",
                "placeholders": [
                    {"id": "pivot-table-columns", "items": []},
                    {"id": "rows", "items": [
                        {"guid": "metric", "title": "metric", "type": "DIMENSION", "data_type": "string"}
                    ]},
                    {"id": "measures", "items": [measure_settings]}
                ]
            },
            "datasetsIds": ["ds"],
            "colors": [],
            "extraSettings": {},
            "datasetsPartialFields": [[]]
        })
    }

    fn settings_response(rows: Value, fields: Value, structure: Value) -> Value {
        json!({
            "fields": fields,
            "pivot": {"structure": structure},
            "pivot_data": {
                "columns_with_info": [
                    {"cells": [], "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}}
                ],
                "row_dimension_headers": [[["metric", 0, 0]]],
                "rows": rows
            }
        })
    }

    fn settings_fields() -> Value {
        json!([
            {"legend_item_id": 0, "id": "metric", "title": "metric", "data_type": "string", "item_type": "field"},
            {"legend_item_id": 1, "id": "m1", "title": "Orders", "data_type": "integer", "item_type": "field"},
            {"legend_item_id": 2, "id": "cf", "title": "cf", "data_type": "float", "item_type": "field"}
        ])
    }

    fn settings_structure(annotation: Option<Value>) -> Value {
        let mut structure = vec![
            json!({"pivot_item_id": 0, "legend_item_ids": [0],
                   "role_spec": {"role": "pivot_row", "direction": "asc"}, "title": "metric"}),
            json!({"pivot_item_id": 1, "legend_item_ids": [1],
                   "role_spec": {"role": "pivot_measure"}, "title": "Orders"}),
        ];
        if let Some(annotation) = annotation {
            structure.push(annotation);
        }
        Value::Array(structure)
    }

    fn settings_row(header: &str, value: &str) -> Value {
        json!({
            "header_with_info": {
                "cells": [[[header, 0, 0]]],
                "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}
            },
            "values": [[[value, 1, 1]]]
        })
    }

    fn measure_with(bars: Option<Value>, background: Option<Value>) -> Value {
        let mut item = json!({"guid": "m1", "title": "Orders", "type": "MEASURE", "data_type": "integer"});
        if let Some(bars) = bars {
            item["barsSettings"] = bars;
        }
        if let Some(background) = background {
            item["backgroundSettings"] = background;
        }
        item
    }

    fn cell_for_field(data: &Value, guid: &str) -> Value {
        data["rows"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row["cells"].as_array().unwrap())
            .find(|cell| {
                cell.get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.starts_with(&format!("fieldId={guid}__")))
                    .unwrap_or(false)
            })
            .cloned()
            .unwrap_or_else(|| panic!("no {guid} cell in {data}"))
    }

    #[test]
    fn build_request_asks_for_the_background_color_annotation() {
        let shared = settings_shared(measure_with(
            None,
            Some(json!({
                "enabled": true,
                "colorFieldGuid": "cf",
                "settings": {
                    "isContinuous": true,
                    "paletteState": {},
                    "gradientState": {"gradientMode": "2-point"}
                }
            })),
        ));
        let request = build_request(&shared, &json!({}), "ds", &json!("rev"), &json!([])).unwrap();
        let annotation = request["pivot"]["structure"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item.pointer("/role_spec/annotation_type").and_then(Value::as_str)
                    == Some("background-color")
            })
            .expect("background-color annotation");
        assert_eq!(annotation["role_spec"]["role"], json!("pivot_annotation"));
        assert_eq!(annotation["role_spec"]["target_legend_item_ids"], json!([1]));
        assert_eq!(annotation["legend_item_ids"], json!([2]));
        let color_field = request["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field.get("legend_item_id").and_then(Value::as_i64) == Some(2))
            .expect("colour field is requested");
        assert_eq!(color_field["ref"], json!({"type": "id", "id": "cf"}));
    }

    #[test]
    fn bars_one_color_sets_value_color_and_view_options() {
        let shared = settings_shared(measure_with(
            Some(json!({
                "enabled": true,
                "showLabels": true,
                "scale": {"mode": "auto"},
                "colorSettings": {"colorType": "one-color", "settings": {"color": "#00FF00"}},
                "showBarsInTotals": false,
            })),
            None,
        ));
        let response = settings_response(
            json!([settings_row("a", "10"), settings_row("b", "20")]),
            settings_fields(),
            settings_structure(None),
        );
        let data = build_data(&shared, &json!({}), &response, "ru").unwrap();
        let cell = cell_for_field(&data, "m1");
        assert_eq!(cell["type"], json!("number"));
        assert_eq!(cell["value"], json!(10));
        assert_eq!(cell["barColor"], json!("#00FF00"));
        assert_eq!(cell["view"], json!("bar"));
        assert_eq!(cell["showLabel"], json!(true));
        assert_eq!(cell["showBar"], json!(true), "{cell}");
        // Auto scale: min 10 is positive, so only `max` is kept.
        assert_eq!(cell["max"], json!(20), "{cell}");
        assert!(cell.get("min").is_none(), "{cell}");
    }

    #[test]
    fn bars_two_color_uses_palette_indices() {
        let shared = settings_shared(measure_with(
            Some(json!({
                "enabled": true,
                "showLabels": false,
                "scale": {"mode": "auto"},
                "colorSettings": {"colorType": "two-color", "settings": {
                    "positiveColorIndex": 2, "negativeColorIndex": 1
                }},
                "showBarsInTotals": false,
            })),
            None,
        ));
        let response = settings_response(
            json!([settings_row("a", "10"), settings_row("b", "-5")]),
            settings_fields(),
            settings_structure(None),
        );
        let data = build_data(&shared, &json!({}), &response, "ru").unwrap();
        let positive = data["rows"][0]["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cell| cell["fieldId"] == json!("m1"))
            .unwrap()
            .clone();
        let negative = data["rows"][1]["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cell| cell["fieldId"] == json!("m1"))
            .unwrap()
            .clone();
        // `default20` scheme: index 2 is green, index 1 is red.
        assert_eq!(positive["barColor"], json!("#7FD169"));
        assert_eq!(negative["barColor"], json!("#FF6A59"));
        // Auto scale with a negative value keeps both bounds.
        assert_eq!(positive["min"], json!(-5), "{positive}");
        assert_eq!(positive["max"], json!(10), "{positive}");
    }

    #[test]
    fn bars_gradient_colors_the_endpoints() {
        let shared = settings_shared(measure_with(
            Some(json!({
                "enabled": true,
                "scale": {"mode": "auto"},
                "colorSettings": {"colorType": "gradient", "settings": {
                    "gradientType": "2-point",
                    "palette": "blue",
                    "reversed": false,
                    "thresholds": {"mode": "auto"}
                }},
                "showBarsInTotals": false,
            })),
            None,
        ));
        let response = settings_response(
            json!([settings_row("a", "0"), settings_row("b", "10")]),
            settings_fields(),
            settings_structure(None),
        );
        let data = build_data(&shared, &json!({}), &response, "ru").unwrap();
        assert_eq!(data["rows"][0]["cells"][1]["barColor"], json!("rgb(0, 68, 163)"));
        assert_eq!(data["rows"][1]["cells"][1]["barColor"], json!("rgb(140, 203, 255)"));
    }

    #[test]
    fn continuous_background_colors_rows_by_the_annotation_value() {
        let shared = settings_shared(measure_with(
            None,
            Some(json!({
                "enabled": true,
                "colorFieldGuid": "cf",
                "settings": {
                    "isContinuous": true,
                    "paletteState": {},
                    "gradientState": {
                        "reversed": false,
                        "gradientMode": "2-point",
                        "thresholdsMode": "auto",
                        "gradientPalette": "blue"
                    }
                }
            })),
        ));
        let response = settings_response(
            json!([
                {"header_with_info": {
                    "cells": [[["a", 0, 0]]],
                    "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                 "values": [[["10", 1, 1], ["0", 2, 5]]]},
                {"header_with_info": {
                    "cells": [[["b", 0, 0]]],
                    "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                 "values": [[["20", 1, 1], ["10", 2, 5]]]}
            ]),
            settings_fields(),
            settings_structure(Some(json!({
                "pivot_item_id": 5, "legend_item_ids": [2],
                "role_spec": {"role": "pivot_annotation", "annotation_type": "background-color"}
            }))),
        );
        let data = build_data(&shared, &json!({}), &response, "ru").unwrap();
        let first = cell_for_field(&data, "m1");
        assert_eq!(first["colorKey"], json!("0"));
        assert_eq!(first["css"]["backgroundColor"], json!("rgb(0, 68, 163)"));
        assert_eq!(first["css"]["color"], json!("#FFF"));
        let second = data["rows"][1]["cells"][1].clone();
        assert_eq!(second["css"]["backgroundColor"], json!("rgb(140, 203, 255)"));
    }

    #[test]
    fn discrete_background_maps_the_annotation_value_through_mounted_colors() {
        let shared = settings_shared(measure_with(
            None,
            Some(json!({
                "enabled": true,
                "colorFieldGuid": "cf",
                "settings": {
                    "isContinuous": false,
                    "paletteState": {"palette": "classic20", "mountedColors": {"a": "1", "b": "2"}},
                    "gradientState": {}
                }
            })),
        ));
        let response = settings_response(
            json!([
                {"header_with_info": {
                    "cells": [[["a", 0, 0]]],
                    "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                 "values": [[["10", 1, 1], ["a", 2, 5]]]},
                {"header_with_info": {
                    "cells": [[["b", 0, 0]]],
                    "header_info": {"sorting_direction": null, "role_spec": {"role": "data"}}},
                 "values": [[["20", 1, 1], ["b", 2, 5]]]}
            ]),
            settings_fields(),
            settings_structure(Some(json!({
                "pivot_item_id": 5, "legend_item_ids": [2],
                "role_spec": {"role": "pivot_annotation", "annotation_type": "background-color"}
            }))),
        );
        let data = build_data(&shared, &json!({}), &response, "ru").unwrap();
        assert_eq!(data["rows"][0]["cells"][1]["css"]["backgroundColor"], json!("#FF3D64"));
        assert_eq!(data["rows"][1]["cells"][1]["css"]["backgroundColor"], json!("#8AD554"));
    }

    #[test]
    fn hierarchy_in_colors_is_expanded_to_the_current_level_field() {
        let mut shared = drill_shared();
        shared["colors"] = json!([{
            "guid": "cat-hierarchy", "title": "Category", "type": "DIMENSION",
            "data_type": "hierarchy", "fields": [
                {"guid": "cat1", "title": "Level 1", "type": "DIMENSION", "data_type": "string"},
                {"guid": "cat2", "title": "Level 2", "type": "DIMENSION", "data_type": "date"}
            ]
        }]);
        let request = build_request(&shared, &json!({}), "ds", &json!("rev"), &drill_schema()).unwrap();
        let color_annotation = request["pivot"]["structure"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item.pointer("/role_spec/annotation_type").and_then(Value::as_str) == Some("color")
            })
            .expect("color annotation");
        assert_eq!(color_annotation["legend_item_ids"], json!([0]));
        let annotation_field = request["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field.get("legend_item_id").and_then(Value::as_i64) == Some(0))
            .unwrap();
        assert_eq!(annotation_field["ref"], json!({"type": "id", "id": "cat1"}));
    }
}
