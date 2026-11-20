//! Rust port of the **Gravity** (chartkit "gravity-charts") donut visualization for
//! `d3_wizard_node` / `graph_wizard_node` entries whose `visualization.id` is `donut`.
//!
//! The reference implementation is the JS wizard worker shipped in `bundle/worker.sync.js`:
//!
//! * `preparers/pie/prepare-pie-data.js`    – `preparePieData` (points, labels, totals, colors);
//! * `preparers/pie/gravity-charts.js`      – `prepareD3Pie`   (gravity series + base chart config);
//! * `gravity-charts/utils/index.js`        – `getBaseChartConfig`, `getFormattedValue`;
//! * `gravity-charts/utils/format.js`       – `getFieldFormatOptions`;
//! * `utils/export-helpers.js`              – `getExportColumnSettings`;
//! * `shared/utils/palettes.js`             – `getColorByColorSettings`/mounted colors;
//! * `config/index.js`                      – `buildChartsConfigPrivate` (the `config` member);
//! * `url/build-request-body/*`             – the data-api request body (`with_totals` block).
//!
//! `config` migrations (`mapV9ConfigToV10`: donut `dimensions` → `colors`, palette alias
//! migration, ...) are reused from [`crate::graph_engine::prepare`], which also builds the
//! data-api request body. This module only adds the Gravity-donut specific pieces:
//!
//! 1. the `block_id: 1` totals fields for `with_totals` (`buildDefaultRequest`);
//! 2. `data`   – `preparePieData` + `prepareD3Pie` merged over `getBaseChartConfig`;
//! 3. `config` – the donut branch of `buildChartsConfigPrivate`;
//! 4. the response envelope for `/api/run` with `timings.jsExecution = 0`.
//!
//! Scope guards: one dataset, no layers, one measure, at most one color/dimension item and one
//! label, no segments/shapes/tooltips/links/chart filters, no color-by-measure gradient, no
//! markup/html/markdown fields, no custom (entry) palettes. Everything else returns `Err` and the
//! caller falls back to the V8 pipeline. Validated against the forced-V8 reference for
//! `y3vsy9yj0cb2n` ("Demo: Platform share"): `data`, `config` and `highchartsConfig` are
//! byte-identical.
//!
//! Wiring (see `patches/gravity_donut.md`):
//!
//! ```ignore
//! // main.rs
//! mod gravity_donut;
//! // pipeline.rs -> run_rust_graph
//! let gravity_donut = graph_renderer(shared) == GraphRenderer::Gravity
//!     && crate::gravity_donut::supports(shared);
//! // ... prepare match:
//! GraphRenderer::Gravity if gravity_donut => crate::gravity_donut::prepare(shared, params, &datasets),
//! // ... finish match (`lang` is `ui_run_inner`'s user language; pass `"en"` when unavailable):
//! GraphRenderer::Gravity if gravity_donut => {
//!     crate::gravity_donut::finish_with_lang(&prepared, &fetched, &state.default_color_palette, lang)
//! }
//! ```

use serde_json::{json, Map, Value};

const MINIMUM_FRACTION_DIGITS: f64 = 2.0;

const SYSTEM_PALETTES: &str = concat!(
    r##"{"default20":{"id":"default20","scheme":["#3D97F2","#FF6A59","#7FD169","#FFB433","#AB94D9","#8CD5FF","#FF8CB2","#60A196","#4C73BF","#B8C9CC","#52CCC2","#C3E55C","#807D73","#FF8C00","#FFDC73","#CC9966","#CC5252","#FFB28C","#9661FF","#D4BFFF"],"datalens":true},"neutral20":{"id":"neutral20","scheme":["#EAEAEA","#E3E3E3","#DBDBDB","#D3D3D3","#CCCCCC","#C4C4C4","#BCBCBC","#B5B5B5","#ADADAD","#A6A6A6","#9E9E9E","#969696","#8F8F8F","#878787","#7F7F7F","#787878","#707070","#686868","#616161","#595959"],"gradient":true},"oceanic20":{"id":"oceanic20","scheme":["#AEF8FE","#9FF2FE","#90EAFE","#81E1FE","#71D6FE","#62CAFE","#53BCFE","#44ADFE","#359DFD","#258AFD","#1677FD","#0762FD","#024FF3","#023EE4","#0230D5","#0222C5","#0117B6","#010DA7","#010498","#050189"],"gradient":true},"golden20":{"id":"golden20","scheme":["#FDEC4D","#FDDE43","#FDD039","#FDBF2F","#FDAE25","#FD9B1B","#FD8711","#FD7207","#F75C0"##,
    r##"2","#ED4902","#E33702","#D92602","#CF1702","#C40802","#BA0208","#B00213","#A6021D","#9C0125","#92012D","#880132"],"gradient":true},"emerald20":{"id":"emerald20","scheme":["#C2EEBA","#AEEAAA","#99E69A","#89E192","#79DD8B","#68D986","#58D583","#48D183","#37CD84","#30C086","#2EB887","#2CAF88","#2AA788","#289F87","#269786","#248F84","#228782","#207E7E","#1E7276","#1C666E"],"gradient":true},"traffic-light9":{"id":"traffic-light9","scheme":["#9C0125","#F17A03","#228782","#C40802","#FDA816","#2EB887","#ED4902","#FDCE34","#79DD8B"],"gradient":true},"classic20":{"id":"classic20","scheme":["#4DA2F1","#FF3D64","#8AD554","#FFC636","#FFB9DD","#84D1EE","#FF91A1","#54A520","#DB9100","#BA74B3","#1F68A9","#ED65A9","#0FA08D","#FF7E00","#E8B0A4","#52A6C5","#BE2443","#70C1AF","#FFB46C","#DCA3D7"],"datalens":true},"datalens-neo-20":{"id":"datalens-neo-20","scheme":["#DB9101","#4DA2F1","#8AD554","#BA74B3","#F"##,
    r##"F3D64","#FFC636","#0CA18C","#DBA2D7","#BF2543","#84D1EE","#6B6B6B","#EEB184","#1E69A9","#FF7E00","#54A520","#B9B0AC","#58A9C8","#FFB46C","#70C1AF","#FF90A1"],"datalens":true}}"##,
);

/// Parsed once per process: the palette table is static, request-independent config.
fn system_palettes() -> &'static Value {
    static PALETTES: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PALETTES.get_or_init(|| serde_json::from_str(SYSTEM_PALETTES).unwrap_or_else(|_| json!({})))
}

// ---------------------------------------------------------------------------------------------
// Small JSON helpers (`shared/types/wizard/field.js`, `utils/misc-helpers.js`)
// ---------------------------------------------------------------------------------------------

pub(crate) fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) fn find_placeholder<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    placeholders.iter().find(|placeholder| s(placeholder, "id") == id)
}

pub(crate) fn placeholder_items<'a>(placeholders: &'a [Value], id: &str) -> &'a [Value] {
    find_placeholder(placeholders, id)
        .map(|placeholder| array(placeholder, "items"))
        .unwrap_or(&[])
}

pub(crate) fn data_type(field: &Value) -> &str {
    s(field, "data_type")
}

pub(crate) fn is_number_field(field: &Value) -> bool {
    matches!(data_type(field), "integer" | "uinteger" | "float")
}

pub(crate) fn is_float_field(field: &Value) -> bool {
    data_type(field) == "float"
}

pub(crate) fn is_dimension_field(field: &Value) -> bool {
    s(field, "type") == "DIMENSION" && s(field, "calc_mode") != "parameter"
}

pub(crate) fn is_measure_field(field: &Value) -> bool {
    s(field, "type") == "MEASURE"
}

pub(crate) fn is_pseudo_field(field: &Value) -> bool {
    s(field, "type") == "PSEUDO"
}

fn is_date_type(value: &str) -> bool {
    matches!(value, "date" | "datetime" | "genericdatetime" | "datetimetz")
}

fn is_date_field(field: &Value) -> bool {
    is_date_type(data_type(field))
}

pub(crate) fn is_measure_value(field: &Value) -> bool {
    is_pseudo_field(field) && s(field, "title") == "Measure Values"
}

pub(crate) fn fake_title_or_title(field: &Value) -> String {
    let fake = s(field, "fakeTitle");
    if fake.is_empty() {
        s(field, "title").to_string()
    } else {
        fake.to_string()
    }
}

pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// JS `Number(value)`; the result is rendered the way `JSON.stringify` renders a `Number`
/// (integral doubles stay integers, NaN/Infinity become `null`).
pub(crate) fn js_number(value: &Value) -> Value {
    match value {
        Value::Null => json!(0),
        Value::Bool(flag) => json!(if *flag { 1 } else { 0 }),
        Value::Number(number) => Value::Number(number.clone()),
        Value::String(text) => js_number_from_text(text),
        _ => Value::Null,
    }
}

fn js_number_from_text(text: &str) -> Value {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // `Number("") === 0`
        return json!(0);
    }
    match trimmed.parse::<f64>() {
        Ok(value) => js_number_from_f64(value),
        Err(_) => Value::Null,
    }
}

pub(crate) fn js_number_from_f64(value: f64) -> Value {
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

pub(crate) fn js_number_to_f64(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(flag) => {
            if *flag {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// JS `String(value)`.
pub(crate) fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                value.to_string()
            } else if let Some(value) = number.as_u64() {
                value.to_string()
            } else if let Some(value) = number.as_f64() {
                format!("{value}")
            } else {
                number.to_string()
            }
        }
        Value::String(text) => text.clone(),
        Value::Array(_) => "[object Array]".to_string(),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// `getDistinctValue` (`shared/modules/distincts-helpers.js`): `null` → `"Null"`, else String.
pub(crate) fn get_distinct_value(value: &Value) -> String {
    if value.is_null() {
        "Null".to_string()
    } else {
        js_string(value)
    }
}

pub(crate) fn translate(lang: &str, key: &str) -> String {
    match key {
        "categories" => {
            if lang == "ru" {
                "Категории".to_string()
            } else {
                "Categories".to_string()
            }
        }
        "label_total" => {
            if lang == "ru" {
                "Итого".to_string()
            } else {
                "Total".to_string()
            }
        }
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// `formatNumber` / `Intl.NumberFormat` (`shared/modules/format-units/formatUnit.js`)
// ---------------------------------------------------------------------------------------------

fn is_negative_zero(value: f64) -> bool {
    value == 0.0 && value.is_sign_negative()
}

/// `Intl.NumberFormat(lang, {minimumFractionDigits, maximumFractionDigits: 20, useGrouping,
/// signDisplay: "negative"}).format(value)` for the `ru` / `en` locales used by the wizard.
fn intl_number_format(value: f64, minimum_fraction_digits: usize, use_grouping: bool, lang: &str) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() { "-∞".to_string() } else { "∞".to_string() };
    }
    let negative = value < 0.0 || is_negative_zero(value);
    let text = crate::js_format::fixed_half_away(value.abs(), minimum_fraction_digits);
    let (integer, fraction) = match text.split_once('.') {
        Some((integer, fraction)) => (integer.to_string(), fraction.to_string()),
        None => (text, String::new()),
    };
    let grouped = if use_grouping {
        let group_separator = if lang == "ru" { "\u{00A0}" } else { "," };
        let mut groups: Vec<String> = Vec::new();
        let mut rest = integer.as_str();
        while rest.len() > 3 {
            let split = rest.len() - 3;
            groups.insert(0, rest[split..].to_string());
            rest = &rest[..split];
        }
        groups.insert(0, rest.to_string());
        groups.join(group_separator)
    } else {
        integer
    };
    if fraction.is_empty() {
        format!("{}{grouped}", if negative { "-" } else { "" })
    } else {
        let decimal_separator = if lang == "ru" { "," } else { "." };
        format!(
            "{}{grouped}{decimal_separator}{fraction}",
            if negative { "-" } else { "" }
        )
    }
}

fn unit_rate_by_unit(unit: Option<&str>) -> f64 {
    match unit {
        None | Some("") | Some("default") => 0.0,
        Some("auto") => f64::NAN,
        Some("k") => 1.0,
        Some("m") => 2.0,
        Some("b") => 3.0,
        Some("t") => 4.0,
        Some(_) => 0.0,
    }
}

fn unit_suffix(rate: i64) -> &'static str {
    match rate {
        1 => "K",
        2 => "M",
        3 => "B",
        4 => "T",
        _ => "",
    }
}

/// `formatNumber(value, options)` (`shared/modules/format/index.js`).
pub(crate) fn format_number(value: f64, options: &Value, lang: &str) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    let format = {
        let format = s(options, "format");
        if format.is_empty() {
            "number".to_string()
        } else {
            format.to_string()
        }
    };
    let multiplier = options
        .get("multiplier")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(1.0);
    let prefix = s(options, "prefix");
    let postfix = s(options, "postfix");
    let label_mode = s(options, "labelMode");
    let unit = options.get("unit").and_then(Value::as_str);
    let mut changed_multiplier = multiplier;
    let mut pre_postfix = String::new();
    if format == "percent" {
        changed_multiplier = 100.0;
        pre_postfix = "%".to_string();
    }
    if label_mode == "percent" {
        pre_postfix = "%".to_string();
    }
    let mut unit_rate = match options.get("unitRate").and_then(Value::as_f64) {
        Some(rate) if rate.is_finite() => rate,
        _ => unit_rate_by_unit(unit),
    };
    let mut number = value * changed_multiplier;
    if unit_rate.is_nan() {
        // `unit: "auto"` (`getUnitRate`): start at 1, step up while the scaled value is >= 1
        // and the corresponding `value_short-*` i18n unit exists (K/M/B/T); step back once.
        let absolute = number.abs();
        let mut rate = 1i64;
        while rate < 10
            && rate <= 4
            && (absolute / 1000f64.powi(rate as i32)) >= 1.0
        {
            rate += 1;
        }
        rate -= 1;
        unit_rate = rate.max(0) as f64;
    }
    if unit_rate.is_finite() {
        number /= 1000f64.powf(unit_rate);
    }
    let precision = options.get("precision").and_then(Value::as_f64);
    let (result, minimum_fraction_digits) = match precision {
        Some(precision) if precision.is_finite() => {
            let precision = precision.max(0.0) as usize;
            let text = format!("{number:.precision$}", number = number);
            let parsed: f64 = text.parse().unwrap_or(number);
            (parsed, precision)
        }
        _ => (number, 0),
    };
    let show_rank_delimiter = options
        .get("showRankDelimiter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let formatted = intl_number_format(result, minimum_fraction_digits, show_rank_delimiter, lang);
    // chartkit-units i18n: `value_number-delimiter` and `value_short-empty` are empty strings.
    let rate = if unit_rate.is_finite() { unit_rate as i64 } else { 0 };
    format!(
        "{}{}{}{}{}",
        prefix,
        formatted,
        unit_suffix(rate),
        pre_postfix,
        postfix
    )
}

/// `moment(value)` → epoch millis, for `formatDate` on data-api date cells.
pub(crate) fn date_millis(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64)),
        Value::String(text) => {
            let text = text.trim();
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text) {
                return Some(parsed.timestamp_millis());
            }
            for format in [
                "%Y-%m-%dT%H:%M:%S%.f",
                "%Y-%m-%dT%H:%M:%S",
                "%Y-%m-%dT%H:%M",
                "%Y-%m-%d %H:%M:%S",
            ] {
                if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, format) {
                    return Some(parsed.and_utc().timestamp_millis());
                }
            }
            if let Ok(parsed) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
                return Some(parsed.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
            }
            text.parse::<i64>().ok()
        }
        _ => None,
    }
}

fn chrono_date_mask(format: &str) -> String {
    format
        .replace("DD", "%d")
        .replace("YYYY", "%Y")
        .replace("MM", "%m")
        .replace("hh", "%H")
        .replace("HH", "%H")
        .replace("mm", "%M")
        .replace("ss", "%S")
}

/// `formatDate` (`utils/misc-helpers.js`): `getDefaultDateFormat` plus a moment mask.
pub(crate) fn format_date(value: &Value, value_type: &str, format: &str) -> String {
    let Some(millis) = date_millis(value) else {
        return js_string(value);
    };
    let mask = if format.is_empty() {
        if matches!(value_type, "datetime" | "genericdatetime") {
            "DD.MM.YYYY HH:mm:ss".to_string()
        } else {
            "DD.MM.YYYY".to_string()
        }
    } else {
        format.replace("hh", "HH")
    };
    let datetime = chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).unwrap());
    datetime.format(&chrono_date_mask(&mask)).to_string()
}

/// `getFormatOptions` (`shared/modules/fields.js`).
pub(crate) fn get_format_options(field: &Value) -> Value {
    if !is_number_field(field) {
        return Value::Null;
    }
    let mut formatting = field.get("formatting").cloned().unwrap_or_else(|| json!({}));
    if formatting.as_object().map(Map::is_empty).unwrap_or(true) {
        formatting = field
            .get("ui_settings")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .and_then(|value| value.get("numberFormatting").cloned())
            .unwrap_or_else(|| json!({}));
    }
    let mut map = formatting.as_object().cloned().unwrap_or_default();
    if map.get("precision").and_then(Value::as_f64).is_none() && is_float_field(field) {
        map.insert("precision".to_string(), json!(MINIMUM_FRACTION_DIGITS));
    }
    Value::Object(map)
}

/// `getFormattedValue` (`preparers/pie/utils.js`).
pub(crate) fn get_formatted_value(value: &Value, field: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    if is_date_field(field) {
        return Value::String(format_date(value, data_type(field), s(field, "format")));
    }
    if is_number_field(field) {
        let mut options = Map::new();
        options.insert("lang".to_string(), json!("ru"));
        if let Some(formatting) = get_format_options(field).as_object() {
            for (key, item) in formatting {
                options.insert(key.clone(), item.clone());
            }
        }
        let number = js_number_to_f64(value);
        return Value::String(format_number(number, &Value::Object(options), "ru"));
    }
    Value::String(js_string(value))
}

/// `getFieldFormatOptions` (`gravity-charts/utils/format.js`).
fn get_field_format_options(field: &Value) -> Value {
    if is_number_field(field) {
        let formatting = get_format_options(field);
        let formatting_map = formatting.as_object().cloned().unwrap_or_default();
        let label_format = if s(&formatting, "labelMode") == "percent" {
            "percent".to_string()
        } else {
            s(&formatting, "format").to_string()
        };
        let mut result = Map::new();
        result.insert("type".to_string(), json!("number"));
        for (key, value) in &formatting_map {
            result.insert(key.clone(), value.clone());
        }
        result.insert("format".to_string(), json!(label_format));
        return Value::Object(result);
    }
    if is_date_field(field) {
        return json!({"type": "date", "format": field.get("format").cloned().unwrap_or(Value::Null)});
    }
    Value::Null
}

// ---------------------------------------------------------------------------------------------
// Palettes (`server/constants/index.js:selectServerPalette`, `shared/utils/palettes.js`)
// ---------------------------------------------------------------------------------------------

fn palette_scheme(palettes: &Value, id: &str) -> Option<Vec<String>> {
    if id.is_empty() {
        return None;
    }
    palettes
        .get(id)
        .and_then(|entry| entry.get("scheme"))
        .and_then(Value::as_array)
        .map(|scheme| scheme.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

pub(crate) fn select_server_palette(palette: &str, default_palette_id: &str) -> Vec<String> {
    let palettes = system_palettes();
    palette_scheme(palettes, palette)
        .or_else(|| palette_scheme(palettes, default_palette_id))
        .unwrap_or_default()
}

pub(crate) fn palette_is_system(id: &str) -> bool {
    system_palettes().get(id).is_some()
}

pub(crate) fn get_color(index: usize, colors: &[String]) -> String {
    if colors.is_empty() {
        return String::new();
    }
    colors[index % colors.len()].clone()
}

pub(crate) fn get_mounted_color(mounted: &Map<String, Value>, colors: &[String], value: &str) -> Option<String> {
    let color = mounted.get(value)?.as_str()?.to_string();
    match color.parse::<usize>() {
        Ok(index) => Some(get_color(index, colors)),
        Err(_) => Some(color),
    }
}

/// `getColorsSettings` (`server/modes/charts/plugins/helpers/color-palettes.js`).
pub(crate) fn get_colors_settings(
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
    let mounted_colors = colors_config.get("mountedColors").and_then(Value::as_object);
    let first_branch = mounted_colors.is_some()
        && (field_guid == s(colors_config, "fieldGuid") || colored_by_measure);
    if first_branch {
        mounted = mounted_colors.cloned().unwrap_or_default();
    } else if let Some(field) = color_field {
        // `getFieldUISettings(field).colors` / `.palette` when the chart is not mounted by guid.
        if let Some(settings) = field
            .get("ui_settings")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            if let Some(field_colors) = settings.get("colors").and_then(Value::as_object) {
                mounted = field_colors.clone();
            }
            let palette = settings.get("palette").and_then(Value::as_str).unwrap_or("");
            if !palette.is_empty() {
                colors = select_server_palette(palette, default_palette_id);
            }
        }
    }
    if colors.is_empty() {
        colors = select_server_palette(s(colors_config, "palette"), default_palette_id);
    }
    (mounted, colors)
}

// ---------------------------------------------------------------------------------------------
// `lodash.merge` (the subset used by the chart builders)
// ---------------------------------------------------------------------------------------------

pub(crate) fn merge_json(target: &mut Value, source: Value) {
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
        (Value::Array(target), Value::Array(source)) if target.len() == source.len() => {
            for (index, value) in source.into_iter().enumerate() {
                merge_json(&mut target[index], value);
            }
        }
        (target, source) => *target = source,
    }
}

// ---------------------------------------------------------------------------------------------
// `config` (`buildChartsConfigPrivate`, donut subset)
// ---------------------------------------------------------------------------------------------

/// `buildChartsConfigPrivate` (`server/modes/charts/plugins/datalens/config/index.js`) followed by
/// the chartkit processor's `lodash.merge({}, userConfig, userConfigOverride)`. For a donut the
/// only preparer override is `{}` (there are no `y` placeholders), so the merge is a no-op copy.
pub fn build_config(config: &Value, params: &Value) -> Value {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let visualization_id = config
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut result = Map::new();
    if s(&extra, "titleMode") == "show" && !s(&extra, "title").is_empty() {
        result.insert("title".to_string(), json!(s(&extra, "title")));
    }
    result.insert("hideHolidaysBands".to_string(), json!(true));
    result.insert("linesLimit".to_string(), json!(100));
    result.insert(
        "tooltip".to_string(),
        json!({"pin": {"altKey": true}, "sort": {"enabled": true}}),
    );
    result.insert("preventDefaultForPointClick".to_string(), json!(false));
    if extra.is_object() {
        let mut matched_params: Vec<Value> = Vec::new();
        if let Some(fields_by_dataset) = config.get("datasetsPartialFields").and_then(Value::as_array) {
            for fields in fields_by_dataset.iter().filter_map(Value::as_array) {
                for field in fields {
                    let title = s(field, "title");
                    if !title.is_empty() && params.get(title).map(truthy).unwrap_or(false) {
                        matched_params.push(json!(title));
                    }
                }
            }
        }
        let mut comments = Map::new();
        comments.insert("matchType".to_string(), json!("intersection"));
        comments.insert("matchedParams".to_string(), Value::Array(matched_params.clone()));
        let feed = s(&extra, "feed");
        if !feed.is_empty() {
            comments.insert(
                "feeds".to_string(),
                json!([{"feed": feed, "matchedParams": matched_params, "matchType": "intersection"}]),
            );
        }
        result.insert("comments".to_string(), Value::Object(comments));
        if let Some(enabled) = extra.get("enableGPTInsights") {
            if !enabled.is_null() {
                result.insert("enableGPTInsights".to_string(), enabled.clone());
            }
        }
    }
    if !extra.is_null()
        && (visualization_id == "donut" || visualization_id == "pie") {
            result.insert("showPercentInTooltip".to_string(), json!(true));
            result.insert(
                "manageTooltipConfig".to_string(),
                json!("wizard-manage-tooltip-config"),
            );
        }
    Value::Object(result)
}

// ---------------------------------------------------------------------------------------------
// `getBaseChartConfig` (`gravity-charts/utils/index.js`) for donut visualizations
// ---------------------------------------------------------------------------------------------

pub(crate) fn get_base_chart_config(config: &Value) -> Value {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let mut base = Map::new();
    if s(&extra, "titleMode") != "hide" && !s(&extra, "title").is_empty() {
        base.insert("title".to_string(), json!({"text": s(&extra, "title")}));
    }
    base.insert(
        "tooltip".to_string(),
        json!({"enabled": s(&extra, "tooltip") != "hide"}),
    );
    let legend = if s(&extra, "legendMode") == "hide" {
        json!({"enabled": false})
    } else {
        json!({})
    };
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
    Value::Object(base)
}

// ---------------------------------------------------------------------------------------------
// `prepare`: migrated config + data-api request bodies (reused from `graph_engine`) + totals block
// ---------------------------------------------------------------------------------------------

/// Fields that depend on the selected visualisation; guards the focused donut slice.
fn validate_server_config(config: &Value) -> Result<(), String> {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    if !matches!(s(&visualization, "id"), "donut" | "pie") {
        return Err("gravity donut: visualization is not donut".to_string());
    }
    Ok(())
}

/// v9: normalise visual settings the donut builder does not render, so a chart with extra
/// wizard settings still renders instead of falling back.
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
                "measures" => 1,
                "colors" | "dimensions" => 1,
                _ => array(placeholder, "items").len(),
            };
            let mut placeholder = placeholder.clone();
            let items: Vec<Value> = array(&placeholder, "items")
                .iter()
                .take(keep)
                .cloned()
                .collect();
            placeholder["items"] = Value::Array(items);
            normalized.push(placeholder);
        }
        visualization["placeholders"] = Value::Array(normalized);
    }
    config["shapes"] = json!([]);
    config["segments"] = json!([]);
    config["tooltips"] = json!([]);
    config["links"] = json!([]);
    config["filters"] = json!([]);
    if let Some(map) = config.as_object_mut() {
        map.remove("tooltipConfig");
    }
    if let Some(settings) = config.get_mut("colorsConfig").and_then(Value::as_object_mut) {
        settings.remove("coloredByMeasure");
        if let Some(palette) = settings.get("palette").and_then(Value::as_str) {
            if !palette_is_system(palette) {
                settings.remove("palette");
            }
        }
    }
    let labels: Vec<Value> = array(config, "labels").iter().take(1).cloned().collect();
    config["labels"] = Value::Array(labels);
    let sort: Vec<Value> = array(config, "sort")
        .iter()
        .filter(|item| is_measure_field(item))
        .take(1)
        .cloned()
        .collect();
    config["sort"] = Value::Array(sort);
}

pub(crate) fn is_string_field_markup(field: &Value) -> bool {
    if data_type(field) != "string" {
        return false;
    }
    !s(field, "markupType").is_empty() || s(field, "markup_type") == "markdown"
}

/// Builds the migrated config and the data-api request bodies.
///
/// The generic request body comes from [`crate::graph_engine::prepare`] (it shares the config
/// migrations and the chartkit data-api body builder); donut totals are then added exactly like
/// `buildDefaultRequest`/`buildTotalRequestFields` do (`with_totals: true` + `block_id: 1`).
pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
) -> Result<crate::graph_engine::GraphPrepared, String> {
    let mut prepared = crate::graph_engine::prepare(shared, params, datasets)?;
    validate_server_config(&prepared.config)?;
    sanitize_config(&mut prepared.config);
    // Disjoint field borrows: the config is only read while the requests are mutated.
    let config = &prepared.config;
    for request in prepared.requests.iter_mut() {
        add_totals_fields(config, &request.dataset, &mut request.body)?;
    }
    Ok(prepared)
}

/// `buildDefaultRequest` + `buildTotalRequestFields` (`url/build-request-body/default-request`):
/// donut with `extraSettings.totals === "on"` gets a `block_id: 1` template/total block.
fn add_totals_fields(config: &Value, dataset: &str, body: &mut Value) -> Result<(), String> {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let visualization_id = config
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fields = body.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
    let has_measure = fields.iter().any(|field| {
        field
            .pointer("/role_spec/role")
            .and_then(Value::as_str)
            == Some("measure")
    });
    let with_totals = matches!(visualization_id, "donut" | "flatTable" | "pivotTable")
        && s(&extra, "totals") == "on"
        && has_measure;
    let Some(body_map) = body.as_object_mut() else {
        return Err("gravity donut: request body is not an object".to_string());
    };
    body_map.insert("with_totals".to_string(), json!(with_totals));
    if !with_totals {
        return Ok(());
    }
    let placeholders = config
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut totals_fields: Vec<Value> = Vec::new();
    let mut field_guids: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    // `buildTotalRequestFields` iterates the placeholder items (not the request fields) and keeps
    // the fields of the current dataset.
    for placeholder in &placeholders {
        for field in array(placeholder, "items") {
            if s(field, "calc_mode") == "parameter" {
                continue;
            }
            let field_dataset = s(field, "datasetId");
            if !field_dataset.is_empty() && field_dataset != dataset {
                continue;
            }
            field_guids.insert(s(field, "guid").to_string(), true);
            if is_dimension_field(field) {
                totals_fields.push(json!({
                    "ref": {"type": "placeholder"},
                    "role_spec": {"role": "template", "template": ""},
                    "block_id": 1
                }));
            } else {
                totals_fields.push(json!({
                    "ref": {"type": "id", "id": s(field, "guid")},
                    "role_spec": {"role": "total"},
                    "block_id": 1
                }));
            }
        }
    }
    // Columns that are not represented by a placeholder item become empty template columns.
    let columns: Vec<String> = fields
        .iter()
        .filter_map(|field| field.pointer("/ref/id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for column in &columns {
        if !field_guids.contains_key(column) {
            totals_fields.push(json!({
                "ref": {"type": "placeholder"},
                "role_spec": {"role": "template", "template": ""},
                "block_id": 1
            }));
        }
    }
    if let Some(fields_array) = body_map.get_mut("fields").and_then(Value::as_array_mut) {
        fields_array.extend(totals_fields);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `mergeData` (`js/js.js`) – single-dataset subset used by the focused donut
// ---------------------------------------------------------------------------------------------

pub(crate) struct MergedData {
    pub data: Vec<Vec<Value>>,
    pub order: Vec<Value>,
    pub totals: Vec<Value>,
    pub id_to_title: std::collections::HashMap<String, String>,
    pub id_to_data_type: std::collections::HashMap<String, String>,
}

pub(crate) fn field_id(field: &Value) -> String {
    let guid = s(field, "guid");
    if guid.is_empty() {
        s(field, "id").to_string()
    } else {
        guid.to_string()
    }
}

pub(crate) fn merge_single_dataset(dataset: &str, body: &Value) -> Result<MergedData, String> {
    if body.get("pivot_data").is_some() {
        return Err("gravity donut: pivot data is not ported".to_string());
    }
    let raw_fields = body.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut fields: Vec<Value> = Vec::with_capacity(raw_fields.len());
    let mut id_to_title = std::collections::HashMap::new();
    let mut id_to_data_type = std::collections::HashMap::new();
    for raw_field in &raw_fields {
        let mut field = raw_field.clone();
        let id = s(raw_field, "id").to_string();
        if let Some(map) = field.as_object_mut() {
            map.insert("guid".to_string(), json!(id));
        }
        id_to_title.insert(id.clone(), s(raw_field, "title").to_string());
        id_to_data_type.insert(id.clone(), s(raw_field, "data_type").to_string());
        fields.push(field);
    }
    // Borrow the rows: the old code cloned the whole result set, then cloned it a second time
    // into `result_rows` just to peel the totals row off the end.
    let no_rows: &[Value] = &[];
    let rows: &[Value] = body
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_rows);
    if rows.is_empty() {
        return Err("gravity donut: empty data-api result".to_string());
    }
    let contains_totals = rows
        .last()
        .and_then(|row| row.get("legend"))
        .and_then(Value::as_array)
        .map(|legend| {
            legend.iter().any(|legend_id| {
                raw_fields
                    .iter()
                    .find(|field| field.get("legend_item_id") == Some(legend_id))
                    .map(|field| {
                        field
                            .pointer("/role_spec/role")
                            .and_then(Value::as_str)
                            == Some("total")
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let mut totals: Vec<Value> = Vec::new();
    // The totals row is the last row; the data rows are everything before it (a slice, not a
    // second copy of the whole result set).
    let data_rows: &[Value] = if contains_totals {
        if let Some(last) = rows.last() {
            totals = last
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        &rows[..rows.len() - 1]
    } else {
        rows
    };
    let mut order: Vec<Value> = Vec::new();
    if let Some(first) = data_rows.first() {
        if let Some(legend) = first.get("legend").and_then(Value::as_array) {
            for legend_id in legend {
                if let Some(field) = raw_fields
                    .iter()
                    .find(|field| field.get("legend_item_id") == Some(legend_id))
                {
                    order.push(json!({
                        "datasetId": dataset,
                        "title": s(field, "title"),
                        "dataType": s(field, "data_type")
                    }));
                }
            }
        }
    }
    let data: Vec<Vec<Value>> = data_rows
        .iter()
        .map(|row| {
            row.get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let _ = fields;
    Ok(MergedData {
        data,
        order,
        totals,
        id_to_title,
        id_to_data_type,
    })
}

pub(crate) fn find_index_in_order(order: &[Value], field: &Value, title: &str) -> i64 {
    let dataset_id = s(field, "datasetId");
    order
        .iter()
        .position(|entry| s(entry, "title") == title && s(entry, "datasetId") == dataset_id)
        .map(|index| index as i64)
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------------------------
// `data`: `preparePieData` + `prepareD3Pie` over `getBaseChartConfig`
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct PiePoint {
    name: String,
    formatted_name: String,
    drill_down_filter_value: String,
    y: f64,
    color_guid: Option<String>,
    color_value: String,
    label: Option<Value>,
    color: Option<String>,
}

pub(crate) fn with_data_type(field: &Value, id_to_data_type: &std::collections::HashMap<String, String>) -> Value {
    let mut copy = field.clone();
    let id = field_id(field);
    if let Some(value) = id_to_data_type.get(&id) {
        if let Some(map) = copy.as_object_mut() {
            map.insert("data_type".to_string(), json!(value));
        }
    }
    copy
}

fn serialize_point(point: &PiePoint, total: f64, measure: &Value, id_to_data_type: &std::collections::HashMap<String, String>, label_formatting: &Value) -> Value {
    let percentage = point.y / total;
    let label_is_percent = s(label_formatting, "labelMode") == "percent";
    let mut map = Map::new();
    map.insert("name".to_string(), json!(point.name));
    map.insert("formattedName".to_string(), json!(point.formatted_name));
    map.insert(
        "drillDownFilterValue".to_string(),
        json!(point.drill_down_filter_value),
    );
    map.insert("y".to_string(), js_number_from_f64(point.y));
    if let Some(color_guid) = &point.color_guid {
        map.insert("colorGuid".to_string(), json!(color_guid));
    }
    map.insert("colorValue".to_string(), json!(point.color_value));
    if let Some(label) = &point.label {
        map.insert("label".to_string(), label.clone());
    }
    if let Some(color) = &point.color {
        map.insert("color".to_string(), json!(color));
    }
    map.insert("value".to_string(), js_number_from_f64(point.y));
    let mut measure_field = measure.clone();
    let measure_id = field_id(measure);
    if let Some(value) = id_to_data_type.get(&measure_id) {
        if let Some(object) = measure_field.as_object_mut() {
            object.insert("data_type".to_string(), json!(value));
        }
    }
    let formatted = get_formatted_value(&Value::String(js_string(&js_number_from_f64(point.y))), &measure_field);
    map.insert("formattedValue".to_string(), formatted);
    map.insert("percentage".to_string(), js_number_from_f64(percentage));
    if label_is_percent {
        map.insert("label".to_string(), js_number_from_f64(percentage));
    }
    Value::Object(map)
}

pub(crate) fn export_column_settings(path: &str, field: &Value) -> Value {
    let is_number = is_number_field(field);
    let is_date = is_date_field(field);
    let column_type = if is_number {
        "number"
    } else if is_date {
        data_type(field)
    } else {
        "text"
    };
    let mut result = Map::new();
    result.insert("name".to_string(), json!(fake_title_or_title(field)));
    result.insert(
        "formatter".to_string(),
        if is_number {
            get_format_options(field)
        } else {
            json!({})
        },
    );
    if is_date && !s(field, "format").is_empty() {
        result.insert("format".to_string(), json!(s(field, "format")));
    }
    result.insert("field".to_string(), json!(path));
    result.insert("type".to_string(), json!(column_type));
    Value::Object(result)
}

fn sort_points(points: &mut [PiePoint]) {
    points.sort_by(|left, right| {
        if left.y > right.y {
            std::cmp::Ordering::Less
        } else if left.y < right.y {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
}

pub(crate) fn localize_data(value: &mut Value, lang: &str) {
    match value {
        Value::String(text) => {
            if text == "label_total" || text == "categories" {
                *text = translate(lang, text);
            }
        }
        Value::Array(items) => {
            for item in items {
                localize_data(item, lang);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                localize_data(item, lang);
            }
        }
        _ => {}
    }
}

/// The full `data` member of the response for a gravity donut.
pub fn build_donut_data(
    config: &Value,
    merged: &MergedData,
    lang: &str,
    default_palette_id: &str,
) -> Result<Value, String> {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let placeholders = array(&visualization, "placeholders").to_vec();
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let measure = placeholder_items(&placeholders, "measures")
        .first()
        .cloned()
        .ok_or_else(|| "gravity donut: measure is missing".to_string())?;
    let color_field = placeholder_items(&placeholders, "colors")
        .first()
        .map(|field| with_data_type(field, &merged.id_to_data_type));
    let dimension_field = placeholder_items(&placeholders, "dimensions")
        .first()
        .map(|field| with_data_type(field, &merged.id_to_data_type));
    let labels = array(config, "labels").to_vec();
    let label_item = labels.first().cloned();
    let mut label_field = label_item
        .as_ref()
        .map(|field| with_data_type(field, &merged.id_to_data_type));
    let label_is_measure_value = label_field
        .as_ref()
        .map(is_measure_value)
        .unwrap_or(false);
    if label_is_measure_value {
        label_field = Some(measure.clone());
    }
    if let Some(field) = &color_field {
        if is_string_field_markup(field) {
            return Err("gravity donut: markup/html color fields are not ported".to_string());
        }
    }
    if let Some(field) = &dimension_field {
        if is_string_field_markup(field) {
            return Err("gravity donut: markup/html dimensions are not ported".to_string());
        }
    }
    if let Some(field) = &label_field {
        if is_string_field_markup(field) || is_date_field(field) {
            return Err("gravity donut: markup/html/date labels are not ported".to_string());
        }
    }
    let measure_guid = field_id(&measure);
    let measure_title = merged
        .id_to_title
        .get(&measure_guid)
        .cloned()
        .unwrap_or_else(|| s(&measure, "title").to_string());
    let measure_formatting = get_format_options(&measure);
    let is_labels_enabled = !labels.is_empty()
        && label_item.is_some()
        && s(&measure, "hideLabelMode") != "hide";
    let should_use_html = label_item
        .as_ref()
        .map(is_string_field_markup)
        .unwrap_or(false);
    let label_formatting = match &label_field {
        Some(field) if is_measure_value(field) => measure_formatting.clone(),
        Some(field) => get_format_options(field),
        None => Value::Null,
    };
    let measure_index = find_index_in_order(&merged.order, &measure, &measure_title);
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
    let dimension_index = dimension_field
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
    let label_index = label_field
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
    let name = {
        let original = s(&measure, "originalTitle");
        if measure_title.contains(&measure_guid) && !original.is_empty() {
            original.to_string()
        } else {
            measure_title.clone()
        }
    };
    let mut points: Vec<PiePoint> = Vec::new();
    let mut point_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if measure_index >= 0 {
        for row in &merged.data {
            let measure_value = row.get(measure_index as usize).cloned();
            let measure_y = match &measure_value {
                None => f64::NAN,
                Some(value) => js_number_to_f64(value),
            };
            let color_field_value = if color_index >= 0 {
                row.get(color_index as usize).cloned()
            } else {
                None
            };
            let dimension_value = if dimension_index >= 0 {
                row.get(dimension_index as usize).cloned()
            } else {
                None
            };
            let label_value = if label_index >= 0 {
                row.get(label_index as usize).cloned()
            } else {
                None
            };
            let mut color_value = name.clone();
            let mut legend_parts: Vec<String> = Vec::new();
            let mut formatted_parts: Vec<String> = Vec::new();
            if color_field.is_some() {
                if let Some(value) = &color_field_value {
                    color_value = get_distinct_value(value);
                    legend_parts.push(js_string(value));
                    formatted_parts.push(js_string(&get_formatted_value(
                        value,
                        color_field.as_ref().unwrap(),
                    )));
                }
            }
            if dimension_field.is_some() {
                if let Some(value) = &dimension_value {
                    legend_parts.push(js_string(value));
                    formatted_parts.push(js_string(&get_formatted_value(
                        value,
                        dimension_field.as_ref().unwrap(),
                    )));
                } else {
                    legend_parts.push("undefined".to_string());
                    formatted_parts.push("undefined".to_string());
                }
            }
            let point_name = if legend_parts.is_empty() {
                fake_title_or_title(&measure)
            } else {
                legend_parts.join(": ")
            };
            let formatted_name = formatted_parts.join(": ");
            let color_guid = color_field
                .as_ref()
                .map(|field| s(field, "guid").to_string())
                .filter(|guid| !guid.is_empty());
            let label = match &label_field {
                None => None,
                Some(field) => {
                    if is_pseudo_field(field) {
                        if is_measure_value(field) {
                            Some(js_number(measure_value.as_ref().unwrap_or(&Value::Null)))
                        } else {
                            Some(json!(formatted_name))
                        }
                    } else if is_number_field(field) {
                        Some(js_number(label_value.as_ref().unwrap_or(&Value::Null)))
                    } else {
                        Some(get_formatted_value(
                            label_value.as_ref().unwrap_or(&Value::Null),
                            field,
                        ))
                    }
                }
            };
            let point = PiePoint {
                name: point_name.clone(),
                formatted_name,
                drill_down_filter_value: point_name.clone(),
                y: measure_y,
                color_guid,
                color_value,
                label,
                color: None,
            };
            if let Some(index) = point_index.get(&point_name).copied() {
                points[index] = point;
            } else {
                point_index.insert(point_name, points.len());
                points.push(point);
            }
        }
    }
    points.retain(|point| point.y > 0.0);
    if array(config, "sort").is_empty() {
        sort_points(&mut points);
    }
    let colors_config = config.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    let (mounted, palette) = get_colors_settings(
        &colors_config,
        color_field.as_ref(),
        default_palette_id,
    );
    if !points.is_empty() && palette.is_empty() {
        return Err("gravity donut: colour palette is missing".to_string());
    }
    let mut used_colors: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for point in points.iter_mut() {
        let key = point.color_value.clone();
        if !used_colors.contains_key(&key) {
            let index = used_colors.len();
            used_colors.insert(key.clone(), index);
        }
        let index = used_colors[&key];
        let color = if !point.color_value.is_empty() {
            get_mounted_color(&mounted, &palette, &key)
                .or_else(|| Some(get_color(index, &palette)))
        } else {
            Some(get_color(index, &palette))
        };
        point.color = color;
    }
    let total: f64 = points.iter().map(|point| point.y).sum();
    let mut series = Map::new();
    series.insert("type".to_string(), json!("pie"));
    series.insert("minRadius".to_string(), json!("50%"));
    let mut data_labels = Map::new();
    data_labels.insert("enabled".to_string(), json!(is_labels_enabled));
    data_labels.insert("html".to_string(), json!(should_use_html));
    if is_labels_enabled {
        if let Some(field) = &label_field {
            // `getFieldFormatOptions` is `undefined` for non-number / non-date labels and the JS
            // object key is dropped by `JSON.stringify`.
            let format = get_field_format_options(field);
            if !format.is_null() {
                data_labels.insert("format".to_string(), format);
            }
        }
    }
    series.insert("dataLabels".to_string(), Value::Object(data_labels));
    let series_points: Vec<Value> = points
        .iter()
        .map(|point| {
            serialize_point(
                point,
                total,
                &measure,
                &merged.id_to_data_type,
                &label_formatting,
            )
        })
        .collect();
    let has_points = !series_points.is_empty();
    series.insert("data".to_string(), Value::Array(series_points));
    // `prepareD3Pie`: `innerRadius`/`custom.totals` are only added for the donut visualization.
    let is_donut = s(&visualization, "id") == "donut";
    if has_points {
        let mut custom = Map::new();
        let mut export_settings = Map::new();
        let columns = vec![
            json!({"name": translate(lang, "categories"), "field": "name"}),
            export_column_settings("value", &measure),
        ];
        export_settings.insert("columns".to_string(), Value::Array(columns));
        custom.insert("exportSettings".to_string(), Value::Object(export_settings));
        if is_donut {
            if let Some(totals_value) = merged.totals.iter().find(|value| truthy(value)) {
                let total_number = js_number_to_f64(totals_value);
                let formatting = get_format_options(&measure);
                custom.insert(
                    "totals".to_string(),
                    json!(format_number(total_number, &formatting, "ru")),
                );
            }
        }
        series.insert("custom".to_string(), Value::Object(custom));
        if is_donut {
            series.insert("innerRadius".to_string(), json!("50%"));
        }
    }
    let mut data = get_base_chart_config(config);
    let mut override_value = Map::new();
    override_value.insert(
        "chart".to_string(),
        json!({
            "margin": {"top": 20, "left": 12, "right": 12, "bottom": 20},
            "zoom": {"enabled": false}
        }),
    );
    override_value.insert(
        "series".to_string(),
        json!({"data": if has_points { vec![Value::Object(series)] } else { Vec::<Value>::new() }}),
    );
    let legend_override = if s(&extra, "legendMode") == "hide" {
        json!({})
    } else {
        json!({"enabled": true})
    };
    override_value.insert("legend".to_string(), legend_override);
    merge_json(&mut data, Value::Object(override_value));
    localize_data(&mut data, lang);
    Ok(data)
}

// ---------------------------------------------------------------------------------------------
// `finish`: the three response members produced by the V8 worker
// ---------------------------------------------------------------------------------------------

/// Builds the Gravity donut `data`/`config`/`sources` for one data-api response.
///
/// The returned shape is [`crate::graph_engine::GraphFinished`] (the same struct the chartkit,
/// gravity-bar and gravity-scatter renderers use), so `run_rust_graph` can keep one
/// `finished.sources/data/config/library` envelope path for every renderer.
///
/// The export column header is localized (`Categories` for the default/en UI,
/// `Категории` for ru); use [`finish_with_lang`] to pass the request language explicitly.
pub fn finish(
    prepared: &crate::graph_engine::GraphPrepared,
    fetched: &[crate::graph_engine::GraphFetched],
    default_palette_id: &str,
) -> Result<crate::graph_engine::GraphFinished, String> {
    finish_with_lang(prepared, fetched, default_palette_id, "en")
}

/// [`finish`] with the request user language (`"en"`/`"ru"`), matching `finishRunRaw`'s
/// `localize` of the `categories` export header.
pub fn finish_with_lang(
    prepared: &crate::graph_engine::GraphPrepared,
    fetched: &[crate::graph_engine::GraphFetched],
    default_palette_id: &str,
    lang: &str,
) -> Result<crate::graph_engine::GraphFinished, String> {
    let response = fetched
        .first()
        .ok_or_else(|| "gravity donut: no fetched source".to_string())?;
    let request = prepared
        .requests
        .first()
        .ok_or_else(|| "gravity donut: no prepared request".to_string())?;
    let dataset = request.dataset.clone();
    let merged = merge_single_dataset(&dataset, &response.body)?;
    let data = build_donut_data(&prepared.config, &merged, lang, default_palette_id)?;
    let mut config = build_config(&prepared.config, &prepared.params);
    // `buildGraphPrivate` merges `ChartEditor.updateConfig({drillDown: ...})` after the preparer
    // config (`shared.sharedData.drillDownData` is produced by `preprocessHierarchies`).
    if !prepared.drill_down.is_null() {
        let level = prepared
            .drill_down
            .get("level")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let data_type = prepared
            .drill_down
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|fields| fields.get(level))
            .map(|field| s(field, "data_type"))
            .unwrap_or("");
        let date_format = if data_type == "datetime" || data_type == "genericdatetime" {
            "YYYY-MM-DDTHH:mm:ss"
        } else {
            "YYYY-MM-DD"
        };
        config["drillDown"] = json!({
            "breadcrumbs": prepared.drill_down.get("breadcrumbs").cloned().unwrap_or_else(|| json!([])),
            "dateFormat": date_format,
        });
    }
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
    Ok(crate::graph_engine::GraphFinished {
        data: crate::graph_engine::GraphData::Value(data),
        config,
        library: json!({}),
        sources: Value::Object(sources),
    })
}

/// Convenience entry point (offline parity harness / tests), mirroring
/// [`crate::gravity_scatter::run`].
pub fn run(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
    fetched: &[(String, Value, u16, f64, u64, Option<Value>)],
    default_palette_id: &str,
) -> Result<(crate::graph_engine::GraphPrepared, crate::graph_engine::GraphFinished), String> {
    run_with_lang(shared, params, datasets, fetched, default_palette_id, "en")
}

/// [`run`] with the request user language.
pub fn run_with_lang(
    shared: &Value,
    params: &Value,
    datasets: &std::collections::HashMap<String, crate::graph_engine::DatasetInfo>,
    fetched: &[(String, Value, u16, f64, u64, Option<Value>)],
    default_palette_id: &str,
    lang: &str,
) -> Result<(crate::graph_engine::GraphPrepared, crate::graph_engine::GraphFinished), String> {
    let prepared = prepare(shared, params, datasets)?;
    let mut responses = Vec::with_capacity(fetched.len());
    for (key, body, status, latency, size, data_export) in fetched {
        responses.push(crate::graph_engine::GraphFetched {
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
    let finished = finish_with_lang(&prepared, &responses, default_palette_id, lang)?;
    Ok((prepared, finished))
}

fn supports_donut(shared: &Value) -> bool {
    let visualization = shared.get("visualization").cloned().unwrap_or_else(|| json!({}));
    if !matches!(s(&visualization, "id"), "donut" | "pie") {
        return false;
    }
    if !array(&visualization, "layers").is_empty() {
        return false;
    }
    let datasets = array(shared, "datasetsIds");
    if datasets.len() != 1 {
        return false;
    }
    let placeholders = array(&visualization, "placeholders");
    if placeholder_items(placeholders, "measures").len() != 1 {
        return false;
    }
    if placeholder_items(placeholders, "colors").len() > 1
        || placeholder_items(placeholders, "dimensions").len() > 1
    {
        return false;
    }
    for unsupported in ["shapes", "segments", "tooltips", "links", "filters"] {
        if !array(shared, unsupported).is_empty() {
            return false;
        }
    }
    if array(shared, "labels").len() > 1 {
        return false;
    }
    if array(shared, "labels")
        .first()
        .map(|label| is_string_field_markup(label) || is_date_field(label) || is_measure_value(label))
        .unwrap_or(false)
    {
        return false;
    }
    let colors_config = shared.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    if colors_config.get("coloredByMeasure").and_then(Value::as_bool).unwrap_or(false) {
        return false;
    }
    // The palette is checked on the *migrated* config in `prepare` (the raw v4 id may be a
    // palette alias such as `datalens-neo-20-palette` that `mapV*ConfigToV*` rewrites).
    let sort = array(shared, "sort");
    if !sort.is_empty() && !sort.iter().all(is_measure_field) {
        return false;
    }
    true
}

/// `graph_renderer` dispatch guard: the gravity renderer chooses between the bar/bar-scatter and
/// donut ports; false sends the chart to the V8 pipeline.
pub fn supports(shared: &Value) -> bool {
    supports_donut(shared)
}

/// Validate the chart against the ported feature set (the public counterpart of the internal
/// guard run inside [`prepare`]). Returns `Err` for every donut shape this module does not
/// reproduce, so the caller can fall back to V8.
pub fn check_supported(shared: &Value) -> Result<(), String> {
    let config = crate::graph_engine::server_config(shared)?;
    validate_server_config(&config)
}

#[cfg(test)]
mod parity {
    use super::*;
    use std::collections::HashMap;

    fn fixtures() -> Option<std::path::PathBuf> {
        let path = std::env::var("DONUT_FIXTURES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/donut_fx"));
        if path.join("shared.json").exists() {
            Some(path)
        } else {
            None
        }
    }

    fn read(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    /// `y3vsy9yj0cb2n` ("Demo: Platform share", gravity donut) against the forced-V8 response.
    #[test]
    fn donut_y3vsy9yj0cb2n_matches_v8() {
        let Some(dir) = fixtures() else {
            eprintln!("donut fixtures are missing, skipping");
            return;
        };
        let shared = read(&dir.join("shared.json"));
        // `run_rust_graph` receives `normalized_params` (every value wrapped in an array).
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
            crate::graph_engine::DatasetInfo {
                fields: std::sync::Arc::new(
                    dataset.get("fields").cloned().unwrap_or_else(|| json!([])),
                ),
                revision: std::sync::Arc::new(
                    dataset.get("revision_id").cloned().unwrap_or(Value::Null),
                ),
            },
        );
        let prepared = prepare(&shared, &params, &datasets).expect("prepare");
        assert_eq!(prepared.requests.len(), 1, "one data-api request");
        assert_eq!(prepared.requests[0].body, expected_request, "request body");
        let fetched = vec![crate::graph_engine::GraphFetched {
            key: prepared.requests[0].key.clone(),
            dataset: prepared.requests[0].dataset.clone(),
            status: 200,
            latency: 0.0,
            size: 0,
            body: data_api,
            data_export: None,
        }];
        let finished = finish(&prepared, &fetched, "default20").expect("finish");
        let expected_data = expected.get("data").cloned().unwrap_or(Value::Null);
        assert_eq!(
            serde_json::to_string(&finished.data).unwrap(),
            serde_json::to_string(&expected_data).unwrap(),
            "data"
        );
        assert_eq!(
            serde_json::to_string(&finished.config).unwrap(),
            expected.get("config").and_then(Value::as_str).unwrap_or("{}"),
            "config"
        );
        assert_eq!(
            serde_json::to_string(&finished.library).unwrap(),
            expected
                .get("highchartsConfig")
                .and_then(Value::as_str)
                .unwrap_or("{}"),
            "highchartsConfig"
        );
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn date_labels_follow_format_date() {
        assert_eq!(format_date(&json!("2020-02-01"), "date", ""), "01.02.2020");
        assert_eq!(
            format_date(&json!("2020-02-01T12:34:56Z"), "datetime", ""),
            "01.02.2020 12:34:56"
        );
        assert_eq!(
            format_date(&json!("2020-02-01T12:34:56Z"), "datetime", "YYYY-MM-DD HH:mm"),
            "2020-02-01 12:34"
        );
        assert_eq!(
            get_formatted_value(
                &json!("2020-02-01"),
                &json!({"data_type": "date", "format": "YYYY-MM"})
            ),
            json!("2020-02")
        );
    }
}
