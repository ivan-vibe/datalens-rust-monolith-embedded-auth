//! Rust port of the DataLens chartkit (Highcharts) graph slice of the wizard worker.
//!
//! It reproduces, for `graph_wizard_node` visualizations, exactly what the JS wizard produces in
//! `buildChartConfig` / `buildChart` / `buildLibraryConfig` + the processor merge:
//!
//! * `config`            – `config/index.js:buildChartsConfigPrivate` + the chartkit preparer's
//!                         `userConfigOverride` (`preparers/line/highcharts.js:getConfig`,
//!                         `preparers/bar-x/highcharts.js`);
//! * `highchartsConfig`  – `highcharts.js:buildHighchartsConfigPrivate` merged with the per-layer
//!                         override from `preparers/line/highcharts.js:getHighchartsConfig` and
//!                         `preparers/bar-x/highcharts.js`;
//! * `data`              – `preparers/line/prepare-line-data.js`,
//!                         `preparers/bar-x/prepare-bar-x.js`,
//!                         `js/helpers/layer-chart.js` (combined-chart extend/merge);
//! * the data-api request bodies – `url/build-sources.js`, `url/build-request-body/*`.
//!
//! Reference sources are the compiled JS shipped in the DataLens UI image
//! (`/opt/app/dist/server/modes/charts/plugins/datalens/**`). The config version migrations are
//! `shared/modules/config/wizard/*`.
//!
//! Scope guards: segments, shapes, colors-by-measure, `y2` on layered charts, html/markup fields,
//! gradient legends, non-`auto` axis formatting, drill-down and more than one dataset are not
//! ported; `prepare` returns `Err`, and the caller (`run_rust_graph`) falls back to V8.

use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Palette schemes. Kept identical to `SYSTEM_PALETTES` in `pipeline.rs` (which is private to that
/// module); the data builder needs the same fallback palette resolution as `selectServerPalette`.
const SYSTEM_PALETTES: &str = concat!(
    r##"{"default20":{"id":"default20","scheme":["#3D97F2","#FF6A59","#7FD169","#FFB433","#AB94D9","#8CD5FF","#FF8CB2","#60A196","#4C73BF","#B8C9CC","#52CCC2","#C3E55C","#807D73","#FF8C00","#FFDC73","#CC9966","#CC5252","#FFB28C","#9661FF","#D4BFFF"],"datalens":true},"neutral20":{"id":"neutral20","scheme":["#EAEAEA","#E3E3E3","#DBDBDB","#D3D3D3","#CCCCCC","#C4C4C4","#BCBCBC","#B5B5B5","#ADADAD","#A6A6A6","#9E9E9E","#969696","#8F8F8F","#878787","#7F7F7F","#787878","#707070","#686868","#616161","#595959"],"gradient":true},"oceanic20":{"id":"oceanic20","scheme":["#AEF8FE","#9FF2FE","#90EAFE","#81E1FE","#71D6FE","#62CAFE","#53BCFE","#44ADFE","#359DFD","#258AFD","#1677FD","#0762FD","#024FF3","#023EE4","#0230D5","#0222C5","#0117B6","#010DA7","#010498","#050189"],"gradient":true},"golden20":{"id":"golden20","scheme":["#FDEC4D","#FDDE43","#FDD039","#FDBF2F","#FDAE25","#FD9B1B","#FD8711","#FD7207","#F75C0"##,
    r##"2","#ED4902","#E33702","#D92602","#CF1702","#C40802","#BA0208","#B00213","#A6021D","#9C0125","#92012D","#880132"],"gradient":true},"emerald20":{"id":"emerald20","scheme":["#C2EEBA","#AEEAAA","#99E69A","#89E192","#79DD8B","#68D986","#58D583","#48D183","#37CD84","#30C086","#2EB887","#2CAF88","#2AA788","#289F87","#269786","#248F84","#228782","#207E7E","#1E7276","#1C666E"],"gradient":true},"traffic-light9":{"id":"traffic-light9","scheme":["#9C0125","#F17A03","#228782","#C40802","#FDA816","#2EB887","#ED4902","#FDCE34","#79DD8B"],"gradient":true},"classic20":{"id":"classic20","scheme":["#4DA2F1","#FF3D64","#8AD554","#FFC636","#FFB9DD","#84D1EE","#FF91A1","#54A520","#DB9100","#BA74B3","#1F68A9","#ED65A9","#0FA08D","#FF7E00","#E8B0A4","#52A6C5","#BE2443","#70C1AF","#FFB46C","#DCA3D7"],"datalens":true},"datalens-neo-20":{"id":"datalens-neo-20","scheme":["#DB9101","#4DA2F1","#8AD554","#BA74B3","#F"##,
    r##"F3D64","#FFC636","#0CA18C","#DBA2D7","#BF2543","#84D1EE","#6B6B6B","#EEB184","#1E69A9","#FF7E00","#54A520","#B9B0AC","#58A9C8","#FFB46C","#70C1AF","#FF90A1"],"datalens":true}}"##,
);

/// Parsed once per process: the palette table is static, request-independent config.
pub(crate) fn system_palettes() -> &'static Value {
    static PALETTES: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PALETTES.get_or_init(|| serde_json::from_str(SYSTEM_PALETTES).unwrap_or_else(|_| json!({})))
}

// ---------------------------------------------------------------------------------------------
// shared field predicates (`shared/types/wizard/field.js` and friends)
// ---------------------------------------------------------------------------------------------

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn data_type(field: &Value) -> &str {
    s(field, "data_type")
}

fn is_date_type(data_type: &str) -> bool {
    matches!(data_type, "date" | "datetime" | "genericdatetime" | "datetimetz")
}

fn is_date_field(field: &Value) -> bool {
    is_date_type(data_type(field))
}

fn is_number_field(field: &Value) -> bool {
    matches!(data_type(field), "integer" | "uinteger" | "float")
}

fn is_float_field(field: &Value) -> bool {
    data_type(field) == "float"
}

fn is_numerical_data_type(data_type: &str) -> bool {
    matches!(data_type, "integer" | "uinteger" | "float")
}

fn is_string_field(field: &Value) -> bool {
    data_type(field) == "string"
}

fn is_html_field(field: &Value) -> bool {
    is_string_field(field) && s(field, "markupType") == "html"
}

fn is_measure_field(field: &Value) -> bool {
    s(field, "type") == "MEASURE"
}

fn is_pseudo_field(field: &Value) -> bool {
    s(field, "type") == "PSEUDO"
}

fn is_dimension_field(field: &Value) -> bool {
    s(field, "type") == "DIMENSION" && s(field, "calc_mode") != "parameter"
}

fn is_parameter(field: &Value) -> bool {
    s(field, "calc_mode") == "parameter"
}

fn is_measure_name(field: &Value) -> bool {
    is_pseudo_field(field) && matches!(s(field, "title"), "Measure Names" | "Column Names")
}

fn is_measure_name_or_value(field: &Value) -> bool {
    is_pseudo_field(field) && matches!(s(field, "title"), "Measure Names" | "Measure Values" | "Column Names")
}

fn fake_title(field: &Value) -> String {
    // `getFakeTitleOrTitle` (`shared/modules/fields.js`): `fakeTitle || title`.
    let fake = s(field, "fakeTitle");
    if !fake.is_empty() {
        return fake.to_string();
    }
    s(field, "title").to_string()
}

fn original_or_title(field: &Value) -> String {
    // `getOriginalTitleOrTitle` (`utils/misc-helpers.js`): `originalTitle || title`.
    let original = s(field, "originalTitle");
    if original.is_empty() {
        s(field, "title").to_string()
    } else {
        original.to_string()
    }
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn object<'a>(value: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    value.get(key).and_then(Value::as_object)
}

fn find_placeholder<'a>(placeholders: &'a [Value], id: &str) -> Option<&'a Value> {
    placeholders.iter().find(|placeholder| s(placeholder, "id") == id)
}

fn placeholder_items<'a>(placeholders: &'a [Value], id: &str) -> &'a [Value] {
    find_placeholder(placeholders, id)
        .map(|placeholder| array(placeholder, "items"))
        .unwrap_or(&[])
}

fn all_placeholder_items(placeholders: &[Value]) -> Vec<Value> {
    let mut items = Vec::new();
    for placeholder in placeholders {
        items.extend(array(placeholder, "items").iter().cloned());
    }
    items
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// config migrations (`shared/modules/config/wizard/mapChartsConfigToLatestVersion.js`)
// ---------------------------------------------------------------------------------------------

fn config_version(config: &Value) -> u32 {
    match config.get("version") {
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0) as u32,
        // `mapChartsConfigToLatestVersion`: an absent version means V1, not "unsupported".
        None => 1,
        _ => 0,
    }
}

/// v1 -> v2 (`shared/modules/config/wizard/v1/mapV1ConfigToV2.js`): the old singular
/// `dataset` (or the `datasets` array) becomes `datasetsIds` + `datasetsPartialFields`.
fn map_v1_to_v2(config: &Value) -> Value {
    let schema_of = |dataset: &Value| -> Vec<Value> {
        let schema = dataset
            .get("result_schema")
            .or_else(|| dataset.pointer("/dataset/result_schema"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        schema
            .into_iter()
            .map(|field| json!({"guid": s(&field, "guid"), "title": s(&field, "title")}))
            .collect()
    };
    let datasets = array(config, "datasets").to_vec();
    let mut config = config.clone();
    let (ids, partials): (Vec<Value>, Vec<Value>) = if datasets.is_empty() {
        let dataset = config.get("dataset").cloned().unwrap_or_else(|| json!({}));
        let schema = schema_of(&dataset);
        (vec![json!(s(&dataset, "id"))], vec![Value::Array(schema)])
    } else {
        (
            datasets.iter().map(|dataset| json!(s(dataset, "id"))).collect(),
            datasets
                .iter()
                .map(|dataset| Value::Array(schema_of(dataset)))
                .collect(),
        )
    };
    for key in ["colors", "filters", "hierarchies", "labels", "links", "sort", "tooltips",
                "updates", "shapes"] {
        if config.get(key).is_none() {
            config[key] = json!([]);
        }
    }
    for key in ["colorsConfig", "extraSettings", "geopointsConfig", "shapesConfig"] {
        if config.get(key).is_none() {
            config[key] = json!({});
        }
    }
    if config.get("title").is_none() {
        config["title"] = json!("");
    }
    config["type"] = json!("datalens");
    config["version"] = json!(2);
    config["datasetsIds"] = Value::Array(ids);
    config["datasetsPartialFields"] = Value::Array(partials);
    config
}

/// v3 -> v4 (`shared/modules/config/wizard/v3/mapV3ConfigToV4.js`): `datetime` fields
/// become `genericdatetime`. The label-formatting defaults of that migration are not
/// ported; the field-type rewrite is what the request builder depends on.
fn map_v3_to_v4(config: &Value) -> Value {
    fn migrate(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.get("data_type").and_then(Value::as_str) == Some("datetime") {
                    map.insert("data_type".to_string(), json!("genericdatetime"));
                }
                if map.get("cast").and_then(Value::as_str) == Some("datetime") {
                    map.insert("cast".to_string(), json!("genericdatetime"));
                }
                for item in map.values_mut() {
                    migrate(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(migrate),
            _ => {}
        }
    }
    let mut config = config.clone();
    migrate(&mut config);
    config["version"] = json!(4);
    config
}

fn supports_axis_mode(placeholder_id: &str, visualization_id: &str) -> bool {
    let reversed_xy = matches!(visualization_id, "bar" | "bar100p");
    (placeholder_id == "x" && !reversed_xy) || (placeholder_id == "y" && reversed_xy)
}

fn all_axis_modes_available(field: &Value) -> bool {
    is_number_field(field) || is_date_field(field)
}

fn map_visualization_placeholders<F>(config: &mut Value, mut convert: F)
where
    F: FnMut(&mut Vec<Value>, &str),
{
    let visualization_id = config
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(viz) = config.get_mut("visualization") else { return };
    if let Some(layers) = viz.get("layers").and_then(Value::as_array).cloned() {
        let mut layers = layers;
        for layer in layers.iter_mut() {
            let layer_id = s(layer, "id").to_string();
            if let Some(placeholders) = layer.get("placeholders").and_then(Value::as_array).cloned() {
                let mut placeholders = placeholders;
                convert(&mut placeholders, &layer_id);
                layer["placeholders"] = Value::Array(placeholders);
            }
        }
        viz["layers"] = Value::Array(layers);
    } else if let Some(placeholders) = viz.get("placeholders").and_then(Value::as_array).cloned() {
        let mut placeholders = placeholders;
        convert(&mut placeholders, &visualization_id);
        viz["placeholders"] = Value::Array(placeholders);
    }
}

/// v4 -> v5 (`shared/modules/config/wizard/v4/mapV4ConfigToV5.js`).
fn map_v4_to_v5(config: &Value) -> Value {
    let sort_has_measure = array(config, "sort").iter().any(is_measure_field);
    let mut config = config.clone();
    map_visualization_placeholders(&mut config, |placeholders, visualization_id| {
        for placeholder in placeholders.iter_mut() {
            let placeholder_id = s(placeholder, "id").to_string();
            if !supports_axis_mode(&placeholder_id, visualization_id) {
                continue;
            }
            let mut settings = placeholder
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if settings.get("axisMode").is_none() {
                let continuous = array(placeholder, "items")
                    .first()
                    .map(all_axis_modes_available)
                    .unwrap_or(false)
                    && !sort_has_measure;
                settings.insert(
                    "axisMode".to_string(),
                    json!(if continuous { "continuous" } else { "discrete" }),
                );
            }
            placeholder["settings"] = Value::Object(settings);
        }
    });
    config
}

/// v6 -> v7 (`shared/modules/config/wizard/v6/mapV6ConfigToV7.js`): `axisMode` -> `axisModeMap`.
fn map_v6_to_v7(config: &Value) -> Value {
    let sort_has_measure = array(config, "sort").iter().any(is_measure_field);
    let mut config = config.clone();
    map_visualization_placeholders(&mut config, |placeholders, visualization_id| {
        for placeholder in placeholders.iter_mut() {
            let placeholder_id = s(placeholder, "id").to_string();
            let mut settings = placeholder
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let axis_mode = settings.remove("axisMode");
            if supports_axis_mode(&placeholder_id, visualization_id) && axis_mode.is_some() {
                let mut mode_map = Map::new();
                if let Some(field) = array(placeholder, "items").first() {
                    let continuous = all_axis_modes_available(field) && !sort_has_measure;
                    mode_map.insert(
                        s(field, "guid").to_string(),
                        json!(if continuous { "continuous" } else { "discrete" }),
                    );
                }
                settings.insert("axisModeMap".to_string(), Value::Object(mode_map));
            } else {
                settings.remove("axisModeMap");
            }
            placeholder["settings"] = Value::Object(settings);
        }
    });
    config
}

/// v8 -> v9 (`v8/mapV8ConfigToV9.js`): scatter color-by-dimension becomes shapes.
fn map_v8_to_v9(config: &Value) -> Value {
    let mut config = config.clone();
    if config.pointer("/visualization/id").and_then(Value::as_str) == Some("scatter") {
        if let Some(color) = array(&config, "colors").first() {
            if s(color, "type") == "DIMENSION" {
                config["shapes"] = json!([color]);
            }
        }
    }
    config
}

/// v9 -> v10 (`v9/mapV9ConfigToV10.js`): pie/donut `dimensions` placeholder -> `colors`.
fn map_v9_to_v10(config: &Value) -> Value {
    let mut config = config.clone();
    let id = config.pointer("/visualization/id").and_then(Value::as_str).unwrap_or("").to_string();
    if matches!(id.as_str(), "pie" | "donut" | "pie-d3" | "donut-d3") {
        if let Some(placeholders) = config.pointer_mut("/visualization/placeholders").and_then(Value::as_array_mut) {
            for placeholder in placeholders.iter_mut() {
                if s(placeholder, "id") == "dimensions" {
                    placeholder["id"] = json!("colors");
                    placeholder["type"] = json!("colors");
                    placeholder["required"] = json!(false);
                }
            }
        }
    }
    config
}

/// v11 -> v12 (`v11/mapV11ConfigToV12.js`): `isMarkdown` -> `markupType`.
fn map_v11_to_v12(config: &Value) -> Value {
    fn replace(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.remove("isMarkdown").is_some() {
                    map.insert("markupType".to_string(), json!("markdown"));
                }
                for item in map.values_mut() {
                    replace(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(replace),
            _ => {}
        }
    }
    let mut config = config.clone();
    replace(&mut config);
    config
}

/// v12 -> v13 (`v12/mapV12ConfigToV13.js`): old default palette id -> classic20.
fn map_v12_to_v13(config: &Value) -> Value {
    let mut config = config.clone();
    if config.pointer("/colorsConfig/palette").and_then(Value::as_str) == Some("default-palette") {
        config["colorsConfig"]["palette"] = json!("classic20");
    }
    if let Some(id) = config.pointer("/visualization/id").and_then(Value::as_str).map(str::to_string) {
        if matches!(id.as_str(), "flatTable" | "pivotTable") {
            // Table background/bars palettes; graph charts are unaffected.
        }
    }
    config
}

/// v13 -> v14 (`v13/mapV13ConfigToV14.js`): strip the `-palette` suffix from palette ids.
fn map_v13_to_v14(config: &Value) -> Value {
    fn replace(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(palette)) = map.get("palette").cloned() {
                    map.insert("palette".to_string(), json!(palette.replace("-palette", "")));
                }
                if let Some(Value::String(palette)) = map.get("metricFontColorPalette").cloned() {
                    map.insert(
                        "metricFontColorPalette".to_string(),
                        json!(palette.replace("-palette", "")),
                    );
                }
                for item in map.values_mut() {
                    replace(item);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(replace),
            _ => {}
        }
    }
    let mut config = config.clone();
    replace(&mut config);
    config
}

/// v14 -> v15 (`v14/mapV14ConfigToV15.js` + `modules/config/utils.js`).
fn map_v14_to_v15(config: &Value) -> Value {
    let mut config = config.clone();
    if let Some(id) = config.pointer("/visualization/id").and_then(Value::as_str).map(str::to_string) {
        let mapped = match id.as_str() {
            "line-d3" => "line",
            "bar-y-d3" => "bar",
            "bar-y-100p-d3" => "bar100p",
            "bar-x-d3" => "column",
            "scatter-d3" => "scatter",
            "pie-d3" => "pie",
            "donut-d3" => "donut",
            "treemap-d3" => "treemap",
            other => other,
        };
        config["visualization"]["id"] = json!(mapped);
    }
    config
}

/// `mapChartsConfigToServerConfig` (`utils/config-helpers.js:10-21`).
/// `preprocessHierarchies` (`utils/hierarchy-helpers.js`): replaces every hierarchy pseudo
/// field by the field of the current drill-down level and returns `drillDownData`.
fn preprocess_hierarchies(config: Value, params: &Value) -> Result<(Value, Value), String> {
    fn first_param(params: &Value, key: &str) -> Option<Value> {
        params.get(key).and_then(Value::as_array).and_then(|items| items.first()).cloned()
    }
    let drill_down_level = first_param(params, "drillDownLevel")
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.parse::<f64>().ok(),
            _ => None,
        })
        .unwrap_or(0.0) as usize;
    let drill_down_filters: Option<Vec<Value>> = params
        .get("drillDownFilters")
        .and_then(Value::as_array)
        .map(|items| items.to_vec());
    let mut result = config;
    let mut drill_down = Value::Null;
    let mut color_hierarchy_seen = false;
    let visualization_id = result
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let not_split_by_colors = matches!(visualization_id.as_str(), "pie" | "donut" | "treemap");

    let process_items = |items: &mut Vec<Value>, is_color_container: bool, drill_down: &mut Value, color_hierarchy_seen: &mut bool| -> Result<(), String> {
        let mut index = 0;
        while index < items.len() {
            let field = items[index].clone();
            if s(&field, "data_type") != "hierarchy" {
                index += 1;
                continue;
            }
            let fields: Vec<Value> = field
                .get("fields")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|field| !is_markup_field(field))
                .collect();
            if fields.is_empty() {
                index += 1;
                continue;
            }
            if drill_down.is_null() {
                let breadcrumbs: Vec<Value> = fields
                    .iter()
                    .map(|field| json!(s(field, "title")))
                    .collect();
                let filters = drill_down_filters
                    .clone()
                    .unwrap_or_else(|| fields.iter().map(|_| json!("")).collect());
                *drill_down = json!({
                    "breadcrumbs": breadcrumbs,
                    "filters": filters,
                    "level": drill_down_level,
                    "fields": fields,
                    "isColorDrillDown": is_color_container,
                });
                *color_hierarchy_seen = is_color_container;
            } else if !not_split_by_colors && (is_color_container || *color_hierarchy_seen) {
                *color_hierarchy_seen = true;
                if let Some(object) = drill_down.as_object_mut() {
                    object.insert("isColorDrillDown".to_string(), json!(true));
                }
            }
            let hierarchy_field_index = if fields.len() < drill_down_level {
                fields.len() - 1
            } else {
                drill_down_level
            };
            items[index] = fields[hierarchy_field_index].clone();
            index += 1;
        }
        Ok(())
    };

    let visualization = result.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let layers = visualization
        .get("layers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !layers.is_empty() {
        let mut updated_layers = layers.clone();
        for layer in updated_layers.iter_mut() {
            if let Some(placeholders) = layer.get_mut("placeholders").and_then(Value::as_array_mut) {
                let mut placeholders_value = placeholders.clone();
                for placeholder in placeholders_value.iter_mut() {
                    if let Some(items) = placeholder.get_mut("items").and_then(Value::as_array_mut) {
                        let mut items_value = items.clone();
                        process_items(&mut items_value, false, &mut drill_down, &mut color_hierarchy_seen)?;
                        *items = items_value;
                    }
                }
                *placeholders = placeholders_value;
            }
            if let Some(common) = layer.get_mut("commonPlaceholders").and_then(Value::as_object_mut) {
                for (key, is_color) in [("colors", true), ("shapes", true), ("segments", true)] {
                    if let Some(items) = common.get_mut(key).and_then(Value::as_array_mut) {
                        let mut items_value = items.clone();
                        process_items(&mut items_value, is_color, &mut drill_down, &mut color_hierarchy_seen)?;
                        *items = items_value;
                    }
                }
            }
        }
        if let Some(visualization) = result.get_mut("visualization") {
            visualization["layers"] = Value::Array(updated_layers);
        }
        let mut colors = array(&result, "colors").to_vec();
        let mut shapes = array(&result, "shapes").to_vec();
        let mut segments = array(&result, "segments").to_vec();
        process_items(&mut colors, true, &mut drill_down, &mut color_hierarchy_seen)?;
        process_items(&mut shapes, true, &mut drill_down, &mut color_hierarchy_seen)?;
        process_items(&mut segments, true, &mut drill_down, &mut color_hierarchy_seen)?;
        result["colors"] = Value::Array(colors);
        result["shapes"] = Value::Array(shapes);
        result["segments"] = Value::Array(segments);
        return Ok((result, drill_down));
    }
    let mut placeholders = visualization
        .get("placeholders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for placeholder in placeholders.iter_mut() {
        if let Some(items) = placeholder.get_mut("items").and_then(Value::as_array_mut) {
            let mut items_value = items.clone();
            process_items(&mut items_value, false, &mut drill_down, &mut color_hierarchy_seen)?;
            *items = items_value;
        }
    }
    let mut colors = array(&result, "colors").to_vec();
    let mut shapes = array(&result, "shapes").to_vec();
    let mut segments = array(&result, "segments").to_vec();
    process_items(&mut colors, true, &mut drill_down, &mut color_hierarchy_seen)?;
    process_items(&mut shapes, true, &mut drill_down, &mut color_hierarchy_seen)?;
    process_items(&mut segments, true, &mut drill_down, &mut color_hierarchy_seen)?;
    if let Some(visualization) = result.get_mut("visualization") {
        visualization["placeholders"] = Value::Array(placeholders);
    }
    result["colors"] = Value::Array(colors);
    result["shapes"] = Value::Array(shapes);
    result["segments"] = Value::Array(segments);
    Ok((result, drill_down))
}

pub fn server_config(shared: &Value) -> Result<Value, String> {
    let mut config = shared.clone();
    let mut version = config_version(&config);
    if !(1..=15).contains(&version) {
        return Err(format!("graph: config version {version} is not ported"));
    }
    if version == 1 {
        config = map_v1_to_v2(&config);
        version = 2;
    }
    if version == 2 {
        // mapV2ConfigToV3 only rewrites geolayer sort; the sort is already in the
        // `commonPlaceholders` shape every other visualization uses.
        config["version"] = json!(3);
        version = 3;
    }
    if version == 3 {
        config = map_v3_to_v4(&config);
        version = 4;
    }
    if version == 4 {
        config = map_v4_to_v5(&config);
        version = 5;
    }
    if version == 6 || version == 5 {
        config = map_v6_to_v7(&config);
    }
    if version <= 8 {
        config = map_v8_to_v9(&config);
    }
    if version <= 9 {
        config = map_v9_to_v10(&config);
    }
    if version <= 11 {
        config = map_v11_to_v12(&config);
    }
    if version <= 12 {
        config = map_v12_to_v13(&config);
    }
    if version <= 13 {
        config = map_v13_to_v14(&config);
    }
    if version <= 14 {
        config = map_v14_to_v15(&config);
    }
    config["version"] = json!("15");
    if config.get("links").map(Value::is_array) != Some(true) {
        config["links"] = json!([]);
    }
    Ok(config)
}

// ---------------------------------------------------------------------------------------------
// axes / formatting helpers shared by config, library and data
// ---------------------------------------------------------------------------------------------

fn axis_mode(placeholder: &Value, field: &Value) -> String {
    placeholder
        .pointer("/settings/axisModeMap")
        .and_then(Value::as_object)
        .and_then(|map| map.get(s(field, "guid")))
        .and_then(Value::as_str)
        .unwrap_or("continuous")
        .to_string()
}

fn visualizations_with_layers(visualization: &Value) -> bool {
    visualization
        .get("layers")
        .and_then(Value::as_array)
        .map(|layers| !layers.is_empty())
        .unwrap_or(false)
}

fn is_percent_visualization(id: &str) -> bool {
    matches!(id, "bar100p" | "column100p" | "area100p")
}

fn does_sorting_affect_axis_mode(visualization_id: &str) -> bool {
    !matches!(
        visualization_id,
        "area" | "area100p" | "column100p" | "bar100p"
    )
}

fn get_axis_nulls_settings(value: &Value, visualization_id: &str) -> String {
    let is_area = matches!(visualization_id, "area" | "area100p");
    let default = if is_area { "as-0" } else { "connect" };
    let current = value.as_str().unwrap_or("");
    let valid = !current.is_empty() && !(current == "use-previous" && !is_area);
    if valid {
        current.to_string()
    } else {
        default.to_string()
    }
}

fn default_date_format(value_type: &str) -> &'static str {
    if value_type == "date" {
        "DD.MM.YYYY"
    } else if value_type == "datetimetz" {
        "DD.MM.YYYY HH:mm:ss Z"
    } else {
        "DD.MM.YYYY HH:mm:ss"
    }
}

fn chrono_format(format: &str) -> String {
    format
        .replace("DD", "%d")
        .replace("YYYY", "%Y")
        .replace("MM", "%m")
        .replace("hh", "%H")
        .replace("HH", "%H")
        .replace("mm", "%M")
        .replace("ss", "%S")
}

fn parse_date_ms(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
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

fn format_date_ms(millis: i64, format: &str) -> String {
    let datetime = chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).unwrap());
    datetime.format(&chrono_format(format)).to_string()
}

/// `getDateAxisValue` (`preparers/line/helpers/getXAxisValue.js:6-12`): genericdatetime is read as
/// UTC (`getTimezoneOffsettedTime` compensates the client timezone, the server runs in UTC).
fn date_axis_value(value: &Value, _data_type: &str) -> Option<i64> {
    parse_date_ms(value)
}

fn get_x_axis_mode(config: &Value) -> String {
    // `getXAxisMode` (`shared/modules/wizard/axis-mode.js`): layered charts reduce over layers,
    // the first non-discrete mode wins; `bar`/`bar100p` use the y placeholder as x.
    let Some(visualization) = config.get("visualization") else {
        return "discrete".to_string();
    };
    let visualization_id = s(visualization, "id");
    let x_placeholder_id = if matches!(visualization_id, "bar" | "bar100p") { "y" } else { "x" };
    if visualizations_with_layers(visualization) {
        let layers = array(visualization, "layers");
        let mut x_field: Option<Value> = None;
        if let Some(layer) = layers.first() {
            x_field = placeholder_items(array(layer, "placeholders"), x_placeholder_id).first().cloned();
        }
        let mut result: Option<String> = None;
        for layer in layers {
            let layer_id = s(layer, "id");
            let placeholders = array(layer, "placeholders");
            let Some(placeholder) = find_placeholder(placeholders, x_placeholder_id) else { continue };
            let field = match (&x_field, placeholder_items(placeholders, x_placeholder_id).first()) {
                (Some(field), _) => field.clone(),
                (None, Some(field)) => field.clone(),
                (None, None) => continue,
            };
            let mode = if is_number_field(&field) || is_date_field(&field) {
                let sort_has_measure = array(visualization, "layers")
                    .iter()
                    .flat_map(|layer| array(layer, "sort"))
                    .any(is_measure_field);
                if does_sorting_affect_axis_mode(layer_id) && sort_has_measure {
                    "discrete".to_string()
                } else {
                    axis_mode(placeholder, &field)
                }
            } else {
                "discrete".to_string()
            };
            if result.as_deref() != Some("discrete") {
                if let Some(mode) = Some(mode.clone()) {
                    result = Some(mode);
                }
            }
        }
        return result.unwrap_or_else(|| "continuous".to_string());
    }
    let placeholders = array(visualization, "placeholders");
    let Some(placeholder) = find_placeholder(placeholders, x_placeholder_id) else {
        return "discrete".to_string();
    };
    let Some(field) = array(placeholder, "items").first() else {
        return "discrete".to_string();
    };
    if is_number_field(field) || is_date_field(field) {
        axis_mode(placeholder, field)
    } else {
        "discrete".to_string()
    }
}

fn is_x_axis_reversed(field: &Value, sort: &[Value], visualization_id: &str) -> Value {
    if is_date_field(field) || is_number_field(field) {
        let guid = s(field, "guid");
        if let Some(item) = sort.iter().find(|item| s(item, "guid") == guid) {
            let direction = s(item, "direction");
            if direction == "DESC" || direction.is_empty() {
                return json!(!matches!(visualization_id, "bar" | "bar100p"));
            }
        }
    }
    Value::Null
}

fn is_grid_enabled(settings: &Value) -> bool {
    s(settings, "grid") != "off"
}

fn get_axis_title(settings: &Value, field: &Value) -> Option<Value> {
    match s(settings, "title") {
        "auto" => {
            if field.is_null() {
                None
            } else {
                Some(json!(fake_title(field)))
            }
        }
        "manual" => Some(json!(s(settings, "titleValue"))),
        "off" => Some(Value::Null),
        _ => None,
    }
}

/// `applyPlaceholderSettingsToAxis` (`utils/axis-helpers.js:38-125`).
fn apply_placeholder_settings_to_axis(placeholder: &Value, axis: &mut Map<String, Value>, ignore_title: bool) {
    let Some(settings) = placeholder.get("settings") else { return };
    let items = array(placeholder, "items");
    let first = items.first().cloned().unwrap_or(Value::Null);
    let scale = s(settings, "scale");
    let scale_value = settings.get("scaleValue").cloned().unwrap_or(Value::Null);
    if settings.get("autoscale").and_then(Value::as_bool) == Some(false)
        || (scale == "auto" && s(settings, "scaleValue") == "min-max-0")
    {
        axis.insert(
            "min".to_string(),
            if s(settings, "type") == "logarithmic" { Value::Null } else { json!(0) },
        );
    } else if scale == "manual" {
        axis.insert("endOnTick".to_string(), json!(false));
        let min = scale_value.get(0).and_then(number_value).unwrap_or(0.0);
        let max = scale_value.get(1).and_then(number_value).unwrap_or(0.0);
        if s(settings, "type") != "logarithmic" || min > 0.0 {
            axis.insert("min".to_string(), json!(min));
        }
        if s(settings, "type") != "logarithmic" || max > 0.0 {
            axis.insert("max".to_string(), json!(max));
        }
    }
    if !ignore_title {
        if let Some(title) = get_axis_title(settings, &first) {
            axis.insert("title".to_string(), json!({"text": title}));
        }
    }
    if is_date_field(&first) {
        axis.insert("type".to_string(), json!("datetime"));
    } else {
        axis.insert(
            "type".to_string(),
            json!(if s(settings, "type") == "logarithmic" { "logarithmic" } else { "linear" }),
        );
    }
    if !is_grid_enabled(settings) {
        axis.insert("gridLineWidth".to_string(), json!(0));
        axis.insert("minorGridLineWidth".to_string(), json!(0));
    }
    if s(settings, "axisVisibility") == "hide" {
        axis.insert("visible".to_string(), json!(false));
    }
    if s(settings, "grid") == "on" && s(settings, "gridStep") == "manual" {
        if let Some(step) = settings.get("gridStepValue").and_then(number_value) {
            axis.insert("tickPixelInterval".to_string(), json!(step));
        }
    }
    let labels = axis.entry("labels".to_string()).or_insert_with(|| json!({}));
    match s(settings, "hideLabels") {
        "yes" => labels["enabled"] = json!(false),
        "no" => labels["enabled"] = json!(true),
        _ => {}
    }
    match s(settings, "labelsView") {
        "horizontal" => labels["rotation"] = json!(0),
        "vertical" => {
            labels["rotation"] = json!(90);
            labels["x"] = json!(-3);
        }
        "angle" => {
            labels["rotation"] = json!(45);
            labels["y"] = json!(20);
            labels["x"] = json!(-3);
        }
        _ => {}
    }
}

fn number_value(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn extend_plot_options(visualization_id: &str, plot_options: &mut Map<String, Value>) {
    match visualization_id {
        "column" => {
            plot_options.entry("column".to_string()).or_insert_with(|| json!({}))["stacking"] = json!("normal");
        }
        "bar" => {
            plot_options.entry("bar".to_string()).or_insert_with(|| json!({}))["stacking"] = json!("normal");
        }
        "column100p" => {
            plot_options.entry("column".to_string()).or_insert_with(|| json!({}))["stacking"] = json!("percent");
        }
        "area" => {
            plot_options.insert("area".to_string(), json!({"stacking": "normal"}));
        }
        "bar100p" => {
            plot_options.entry("bar".to_string()).or_insert_with(|| json!({}))["stacking"] = json!("percent");
        }
        "area100p" => {
            plot_options.insert("area".to_string(), json!({"stacking": "percent"}));
        }
        "pie" => {
            plot_options.insert("pie".to_string(), json!({"allowPointSelect": false}));
        }
        "donut" => {
            plot_options.insert(
                "pie".to_string(),
                json!({"allowPointSelect": false, "innerSize": "50%"}),
            );
        }
        _ => {}
    }
    if matches!(visualization_id, "column" | "column100p" | "bar" | "bar100p") {
        let column = plot_options.entry("column".to_string()).or_insert_with(|| json!({}));
        column["dataGrouping"] = json!({"enabled": false});
        column["maxPointWidth"] = json!(50);
        plot_options.entry("bar".to_string()).or_insert_with(|| json!({}));
    }
}

fn axis_type(field: &Value, settings: &Value, axis_mode: &str) -> Option<&'static str> {
    if axis_mode != "discrete" {
        if is_date_field(field) {
            return Some("datetime");
        }
        if is_number_field(field) {
            return Some(if s(settings, "type") == "logarithmic" { "logarithmic" } else { "linear" });
        }
    }
    if !field.is_null() {
        return Some("category");
    }
    None
}

fn get_field_exporting_options(field: &Value) -> Value {
    let format = field
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            if is_date_field(field) {
                Some(default_date_format(data_type(field)).to_string())
            } else {
                None
            }
        });
    let mut result = Map::new();
    result.insert("title".to_string(), json!(fake_title(field)));
    result.insert("dataType".to_string(), json!(data_type(field)));
    if let Some(format) = format {
        result.insert("format".to_string(), json!(format));
    }
    Value::Object(result)
}

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
        (Value::Array(target), Value::Array(source)) if target.len() == source.len() => {
            for (index, value) in source.into_iter().enumerate() {
                merge_json(&mut target[index], value);
            }
        }
        (target, source) => *target = source,
    }
}

// ---------------------------------------------------------------------------------------------
// `config` (`config/index.js:buildChartsConfigPrivate` + chartkit `getConfig`)
// ---------------------------------------------------------------------------------------------

fn build_config_from_server_config(config: &Value, params: &Value) -> Value {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let id = s(&visualization, "id").to_string();
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let mut result = json!({
        "hideHolidaysBands": true,
        "linesLimit": 100,
        "tooltip": {"pin": {"altKey": true}, "sort": {"enabled": true}},
        "preventDefaultForPointClick": false,
    });
    if !extra.is_null() {
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
            result["comments"]["feeds"] = json!([{"feed": feed, "matchedParams": matched, "matchType": "intersection"}]);
        }
    }
    if !extra.is_null() {
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
        } else if id == "metric" {
            result["metricVersion"] = json!(2);
        } else if id == "pivotTable" {
            result["settings"] = json!({"externalSort": true});
        } else if id == "combined-chart" {
            result["manageTooltipConfig"] = json!("wizard-manage-tooltip-config");
        }
    }
    result
}

/// The chartkit data preparer sets `config.precision = 2` when a y placeholder has a `float` item
/// (`preparers/line/highcharts.js:getConfig`, `preparers/bar-x/highcharts.js:49-55`). It is a
/// `userConfigOverride` merged into the base config after the fact.
fn config_override(config: &Value) -> Value {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let layers: Vec<Value> = if visualizations_with_layers(&visualization) {
        array(&visualization, "layers").to_vec()
    } else {
        vec![visualization.clone()]
    };
    let mut override_value = json!({});
    for layer in layers {
        let placeholders = array(&layer, "placeholders");
        let mut has_float = false;
        for placeholder_id in ["y", "y2"] {
            if placeholder_items(placeholders, placeholder_id)
                .iter()
                .any(is_float_field)
            {
                has_float = true;
            }
        }
        if has_float {
            override_value["precision"] = json!(2);
        }
    }
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    if is_navigator_enabled(config) {
        if let Some(navigator_settings) = extra.get("navigatorSettings") {
            override_value["navigatorSettings"] = navigator_settings.clone();
        }
        let has_x = !placeholder_items(
            array(&visualization, "placeholders"),
            "x",
        )
        .is_empty();
        if has_x {
            let series_name = extra.get("navigatorSeriesName").cloned();
            let mut highstock = Map::new();
            if let Some(series_name) = series_name {
                if !series_name.is_null() {
                    highstock.insert("base_series_name".to_string(), series_name);
                }
            }
            override_value["highstock"] = Value::Object(highstock);
        }
    }
    override_value
}

// ---------------------------------------------------------------------------------------------
// `highchartsConfig`
// ---------------------------------------------------------------------------------------------

fn build_highcharts_config(config: &Value) -> Value {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let id = s(&visualization, "id").to_string();
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let layers = array(&visualization, "layers").to_vec();
    let placeholders: Vec<Value> = if layers.is_empty() {
        array(&visualization, "placeholders").to_vec()
    } else {
        layers.iter().flat_map(|layer| array(layer, "placeholders").to_vec()).collect()
    };
    let highcharts_id = s(&visualization, "highchartsId");
    let mut chart = json!({"type": if highcharts_id.is_empty() { id.as_str() } else { highcharts_id }, "zoomType": "x"});
    if id == "combined-chart" {
        chart["type"] = json!("");
    }
    let mut legend = json!({});
    let mut tooltip = json!({});
    let ignore_title = !array(config, "segments").is_empty();
    let x_placeholder = find_placeholder(&placeholders, "x");
    let y_placeholder = find_placeholder(&placeholders, "y");
    let y2_placeholder = find_placeholder(&placeholders, "y2");
    let mut x_axis = Map::new();
    x_axis.insert("endOnTick".to_string(), json!(false));
    if let Some(placeholder) = x_placeholder {
        apply_placeholder_settings_to_axis(placeholder, &mut x_axis, ignore_title);
    }
    let x_axis_value = Value::Object(x_axis);
    let has_y = y_placeholder
        .map(|placeholder| !array(placeholder, "items").is_empty())
        .unwrap_or(false);
    let has_y2 = y2_placeholder
        .map(|placeholder| !array(placeholder, "items").is_empty())
        .unwrap_or(false);
    let y_axis_value;
    if has_y && has_y2 {
        let mut first = Map::new();
        first.insert("opposite".to_string(), json!(false));
        first.insert("labels".to_string(), json!({"y": 3}));
        let mut second = Map::new();
        second.insert("opposite".to_string(), json!(true));
        second.insert("labels".to_string(), json!({"y": 3}));
        let mut axes = vec![first, second];
        if let Some(placeholder) = y_placeholder {
            apply_placeholder_settings_to_axis(placeholder, &mut axes[0], ignore_title);
        }
        if let Some(placeholder) = y2_placeholder {
            apply_placeholder_settings_to_axis(placeholder, &mut axes[1], ignore_title);
        }
        y_axis_value = Value::Array(axes.into_iter().map(Value::Object).collect());
    } else {
        let chosen = if has_y2 { y2_placeholder } else { y_placeholder };
        let mut axis = Map::new();
        axis.insert("opposite".to_string(), json!(!has_y && has_y2));
        axis.insert("labels".to_string(), json!({"y": 3}));
        if let Some(placeholder) = chosen {
            apply_placeholder_settings_to_axis(placeholder, &mut axis, ignore_title);
        }
        y_axis_value = Value::Object(axis);
    }
    let mut y_axis_value = y_axis_value;
    if is_percent_visualization(&id) && y_axis_value.get("max").is_none() {
        y_axis_value["max"] = json!(100);
    }
    let mut plot_options = Map::new();
    let mut visualizations = vec![id.clone()];
    visualizations.extend(layers.iter().map(|layer| s(layer, "id").to_string()));
    for visualization_id in &visualizations {
        match visualization_id.as_str() {
            "line" => {
                chart["zoomType"] = json!("xy");
                legend["symbolWidth"] = json!(38);
            }
            "column" | "column100p" | "bar" | "bar100p" | "combined-chart" => {
                chart["zoomType"] = json!("xy");
                legend["labelFormatter"] = json!("wizard-label-formatter");
            }
            _ => {}
        }
        extend_plot_options(visualization_id, &mut plot_options);
    }
    if id == "area" {
        if s(&extra, "stacking") == "off" {
            plot_options.insert("area".to_string(), json!({}));
        } else {
            plot_options.insert("area".to_string(), json!({"stacking": "normal"}));
        }
    }
    if s(&extra, "legendMode") == "hide" {
        legend = json!({"enabled": false});
    }
    if s(&extra, "tooltip") == "hide" {
        tooltip["enabled"] = json!(false);
    }
    let label_position = s(&extra, "labelsPosition");
    if !label_position.is_empty() && visualizations.iter().any(|id| matches!(id.as_str(), "column" | "bar")) {
        let series = plot_options.entry("series".to_string()).or_insert_with(|| json!({}));
        series["dataLabels"] = json!({"inside": label_position != "outside"});
    }
    let allow_overlap = s(&extra, "overlap") == "on";
    let series = plot_options.entry("series".to_string()).or_insert_with(|| json!({}));
    series["dataGrouping"] = json!({"enabled": false});
    if let Some(data_labels) = series.get_mut("dataLabels").and_then(Value::as_object_mut) {
        data_labels.insert("allowOverlap".to_string(), json!(allow_overlap));
    } else {
        series["dataLabels"] = json!({"allowOverlap": allow_overlap});
    }
    let mut navigator = json!({"series": {"dataLabels": {"color": "transparent"}, "fillOpacity": 0.15}});
    if id == "column" {
        navigator["yAxis"] = json!({"softMin": 0});
    }
    let base = json!({
        "chart": chart,
        "legend": legend,
        "xAxis": x_axis_value,
        "yAxis": y_axis_value,
        "tooltip": tooltip,
        "plotOptions": Value::Object(plot_options),
        "navigator": navigator,
    });
    base
}

fn axis_settings_string(placeholder: &Value) -> Value {
    placeholder.get("settings").cloned().unwrap_or_else(|| json!({}))
}

/// `addAxisFormatting` + `getAxisChartkitFormatting` (`preparers/helpers/axis/*`): the
/// chartkit formatting entry for a placeholder whose `axisFormatMode` is not `auto`.
fn axis_chartkit_formatting(placeholder: &Value, visualization_id: &str) -> Option<Value> {
    let settings = axis_settings_string(placeholder);
    let mode = s(&settings, "axisFormatMode");
    if mode.is_empty() || mode == "auto" {
        return None;
    }
    let field = array(placeholder, "items").first()?;
    if !is_number_field(field) || is_percent_visualization(visualization_id) {
        return None;
    }
    let formatting = match mode {
        "by-field" => get_format_options(field),
        "manual" => settings
            .get("axisLabelFormating")
            .cloned()
            .unwrap_or_else(|| json!({})),
        _ => return None,
    };
    Some(format_options_from_field_formatting(&formatting, data_type(field)))
}

/// `addAxisFormatter` (`preparers/helpers/axis/addAxisFormatter.js`): sets
/// `labels.formatter`/`labels.format` (both are dropped when `undefined`).
fn add_axis_formatter(axis: &mut Map<String, Value>, placeholder: Option<&Value>, otherwise_formatter: Option<&str>) {
    let Some(placeholder) = placeholder else { return };
    let Some(field) = array(placeholder, "items").first() else { return };
    let settings = axis_settings_string(placeholder);
    let axis_label_date_format = s(&settings, "axisFormatMode") == "manual"
        && is_date_field(field)
        && false; // `isAxisLabelDateFormat` also requires axisType === "datetime"
    let formatter = if axis_label_date_format {
        Some("wizard-datetime-axis-formatter")
    } else {
        otherwise_formatter
    };
    let labels = axis.entry("labels".to_string()).or_insert_with(|| json!({}));
    if let Some(formatter) = formatter {
        labels["formatter"] = json!(formatter);
    }
    if axis_label_date_format {
        labels["format"] = json!(s(&settings, "axisLabelDateFormat"));
    }
}

/// `getHighchartsConfig` from `preparers/line/highcharts.js:23-170`.
fn get_navigator_mode(extra: &Value) -> String {
    let mode = extra
        .pointer("/navigatorSettings/navigatorMode")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !mode.is_empty() {
        return mode.to_string();
    }
    s(extra, "navigatorMode").to_string()
}

fn is_navigator_available(visualization: &Value) -> bool {
    let id = s(visualization, "id");
    if !matches!(id, "line" | "area" | "area100p" | "column") {
        return false;
    }
    let placeholders = array(visualization, "placeholders");
    let Some(x_placeholder) = find_placeholder(placeholders, "x") else {
        return false;
    };
    let items = array(x_placeholder, "items");
    let Some(item) = items.first() else { return false };
    if !is_date_field(item) {
        return false;
    }
    let settings = axis_settings_string(x_placeholder);
    let mode = settings
        .pointer("/axisModeMap")
        .and_then(Value::as_object)
        .and_then(|map| map.get(s(item, "guid")))
        .and_then(Value::as_str)
        .unwrap_or("discrete");
    mode != "discrete"
}

fn is_navigator_enabled(config: &Value) -> bool {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    get_navigator_mode(&extra) == "show" && is_navigator_available(&visualization)
}

/// `lodash.mergeWith` used by the processor postprocess: arrays merge with objects by mapping
/// every element (`mergeArrayWithObject(a, b) || mergeArrayWithObject(b, a)`).
fn merge_library_value(target: Value, source: Value) -> Value {
    match (target, source) {
        (Value::Object(mut target), Value::Object(source)) => {
            for (key, value) in source {
                match target.remove(&key) {
                    Some(existing) => {
                        target.insert(key, merge_library_value(existing, value));
                    }
                    None => {
                        target.insert(key, value);
                    }
                }
            }
            Value::Object(target)
        }
        (Value::Array(mut target), Value::Array(source)) => {
            for (index, value) in source.into_iter().enumerate() {
                if index < target.len() {
                    let existing = std::mem::replace(&mut target[index], Value::Null);
                    target[index] = merge_library_value(existing, value);
                } else {
                    target.push(value);
                }
            }
            Value::Array(target)
        }
        (Value::Array(mut target), Value::Object(source)) => {
            for element in target.iter_mut() {
                let existing = std::mem::replace(element, Value::Null);
                *element = merge_library_value(existing, Value::Object(source.clone()));
            }
            Value::Array(target)
        }
        (Value::Object(target), Value::Array(source)) => {
            Value::Array(
                source
                    .into_iter()
                    .map(|element| merge_library_value(Value::Object(target.clone()), element))
                    .collect(),
            )
        }
        (_target, source) => source,
    }
}

fn get_segments_y_axis(
    config: &Value,
    placeholders: &[Value],
    graphs: &Value,
    visualization_id: &str,
) -> (Vec<Value>, Vec<Value>) {
    // Segment titles in graph order (`getSegmentsYAxis` is driven by the sorted segments map).
    let mut segment_titles: Vec<String> = Vec::new();
    if let Some(graphs) = graphs.as_array() {
        for graph in graphs {
            if let Some(title) = graph.pointer("/custom/segmentTitle").and_then(Value::as_str) {
                if !segment_titles.iter().any(|existing| existing == title) {
                    segment_titles.push(title.to_string());
                }
            }
        }
    }
    let y2_placeholder = find_placeholder(placeholders, "y2");
    let has_opposite = !placeholder_items(placeholders, "y2").is_empty();
    let segments_number = segment_titles.len().max(1);
    let taken = 4 * (segments_number as i64 - 1);
    let free = 100 - taken;
    let space = free / segments_number as i64;
    let y_placeholder = find_placeholder(placeholders, "y");
    let mut axes: Vec<Value> = Vec::new();
    let mut formattings: Vec<Value> = Vec::new();
    for (index, title) in segment_titles.iter().enumerate() {
        let is_opposite = false;
        let placeholder = if is_opposite { y2_placeholder } else { y_placeholder };
        let mut axis = Map::new();
        axis.insert("top".to_string(), json!(format!("{}%", 4 * index as i64 + space * index as i64)));
        axis.insert("height".to_string(), json!(format!("{space}%")));
        axis.insert("offset".to_string(), json!(0));
        axis.insert("lineWidth".to_string(), json!(1));
        axis.insert("gridLineWidth".to_string(), json!(1));
        axis.insert("opposite".to_string(), json!(is_opposite));
        if !is_opposite {
            axis.insert(
                "title".to_string(),
                json!({
                    "text": title,
                    "useHTML": false,
                    "align": "middle",
                    "textAlign": "center",
                    "offset": 120,
                    "rotation": 0,
                    "style": {
                        "whiteSpace": "nowrap",
                        "textOverflow": "ellipsis",
                        "fontSize": "14px",
                        "fontWeight": "bold",
                        "width": 120
                    }
                }),
            );
        }
        if let Some(placeholder) = placeholder {
            apply_placeholder_settings_to_axis(placeholder, &mut axis, true);
            add_axis_formatter(&mut axis, Some(placeholder), None);
        }
        axes.push(Value::Object(axis));
        formattings.push(Value::Null);
        let _ = has_opposite;
        let _ = config;
        let _ = visualization_id;
    }
    (axes, formattings)
}

fn line_preparer_overlay(
    config: &Value,
    visualization_id: &str,
    placeholders: &[Value],
    graphs: &Value,
) -> Result<Map<String, Value>, String> {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let x_placeholder = find_placeholder(placeholders, "x");
    let x = x_placeholder.and_then(|placeholder| array(placeholder, "items").first().cloned());
    let x_settings = x_placeholder.map(axis_settings_string).unwrap_or_else(|| json!({}));
    let x_axis_mode = get_x_axis_mode(config);
    let x_axis_type = axis_type(x.as_ref().unwrap_or(&Value::Null), &x_settings, &x_axis_mode);
    let wizard_x_axis_formatter = if x.as_ref().map(is_date_field).unwrap_or(false)
        && x_axis_type == Some("category")
    {
        Some("wizard-x-axis-formatter")
    } else {
        None
    };
    let mut x_axis = Map::new();
    if let Some(axis_type) = x_axis_type {
        x_axis.insert("type".to_string(), json!(axis_type));
    }
    if let Some(x) = &x {
        let reversed = is_x_axis_reversed(x, array(config, "sort"), visualization_id);
        if !reversed.is_null() {
            x_axis.insert("reversed".to_string(), reversed);
        }
    }
    x_axis.insert(
        "labels".to_string(),
        json!({"useHTML": x.as_ref().map(is_html_field).unwrap_or(false)}),
    );
    let mut axes_formatting = json!({"xAxis": [], "yAxis": []});
    if let Some(formatting) = x_placeholder
        .and_then(|placeholder| axis_chartkit_formatting(placeholder, visualization_id))
    {
        axes_formatting["xAxis"] = Value::Array(vec![formatting]);
    }
    add_axis_formatter(&mut x_axis, x_placeholder, wizard_x_axis_formatter);
    let y_placeholder = find_placeholder(placeholders, "y");
    let y2_placeholder = find_placeholder(placeholders, "y2");
    let y_items = placeholder_items(placeholders, "y");
    let y2_items = placeholder_items(placeholders, "y2");
    let merged_y_sections = y_items.len() + y2_items.len();
    let mut part = Map::new();
    if merged_y_sections > 0 {
        if is_navigator_enabled(config) {
            x_axis.insert("ordinal".to_string(), json!(x_axis_mode == "discrete"));
        }
        if s(&extra, "tooltipSum").is_empty() || s(&extra, "tooltipSum") == "on" {
            part.insert("enableSum".to_string(), json!(true));
        }
        if let Some(x) = &x {
            if !is_measure_name_or_value(x) {
                part.insert("tooltipHeaderFormatter".to_string(), json!(fake_title(x)));
            }
        }
        let segments = array(config, "segments");
        if segments.is_empty() {
            let is_last_layer = !visualizations_with_layers(config.get("visualization").unwrap_or(&Value::Null))
                || last_layer_id(config).as_deref() == Some(visualization_id);
            let layer_y_placeholder = if is_last_layer { y_placeholder } else { None };
            let layer_y2_placeholder = if is_last_layer { y2_placeholder } else { None };
            let mut y_axis = Map::new();
            if !y_items.is_empty() && !y2_items.is_empty() {
                let mut y1 = Map::new();
                let mut y2 = Map::new();
                add_axis_formatter(&mut y1, layer_y_placeholder, None);
                add_axis_formatter(&mut y2, layer_y2_placeholder, None);
                part.insert("yAxis".to_string(), Value::Array(vec![Value::Object(y1), Value::Object(y2)]));
            } else if !y_items.is_empty() {
                add_axis_formatter(&mut y_axis, layer_y_placeholder, None);
                part.insert("yAxis".to_string(), Value::Object(y_axis));
            } else {
                add_axis_formatter(&mut y_axis, layer_y2_placeholder, None);
                part.insert("yAxis".to_string(), Value::Object(y_axis));
            }
        } else {
            let legend_enabled = (!array(config, "colors").is_empty()
                || !array(config, "shapes").is_empty()
                || !y_items.is_empty() && y_items.len() > 1
                || y2_items.len() > 1)
                && s(&extra, "legendMode") != "hide";
            part.insert("legend".to_string(), json!({"enabled": legend_enabled}));
            let (segments_y, formattings) = get_segments_y_axis(config, placeholders, graphs, visualization_id);
            part.insert("yAxis".to_string(), Value::Array(segments_y));
            axes_formatting["yAxis"] = Value::Array(formattings);
        }
    }
    let mut final_labels = Map::new();
    final_labels.insert(
        "useHTML".to_string(),
        json!(x.as_ref().map(is_html_field).unwrap_or(false)),
    );
    if let Some(formatter) = wizard_x_axis_formatter {
        final_labels.insert("formatter".to_string(), json!(formatter));
    }
    x_axis.insert("labels".to_string(), Value::Object(final_labels));
    part.insert("xAxis".to_string(), Value::Object(x_axis));
    part.insert("axesFormatting".to_string(), axes_formatting);
    let mut exporting = json!({"csv": {"custom": {}, "columnHeaderFormatter": "wizard-export-column-names-formatter"}});
    if let Some(x) = &x {
        exporting["csv"]["custom"]["categoryHeader"] = get_field_exporting_options(x);
    }
    part.insert("exporting".to_string(), exporting);
    let _ = graphs;
    Ok(part)
}

fn last_layer_id(config: &Value) -> Option<String> {
    config
        .pointer("/visualization/layers")
        .and_then(Value::as_array)
        .and_then(|layers| layers.last())
        .map(|layer| s(layer, "id").to_string())
}

/// `prepareHighchartsBarX` (`preparers/bar-x/highcharts.js:22-187`).
fn bar_x_preparer_overlay(
    config: &Value,
    placeholders: &[Value],
    graphs: &Value,
) -> Result<Map<String, Value>, String> {
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let visualization_id = config
        .pointer("/visualization/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let x_placeholder = find_placeholder(placeholders, "x");
    let x = x_placeholder.and_then(|placeholder| array(placeholder, "items").first().cloned());
    let x_settings = x_placeholder.map(axis_settings_string).unwrap_or_else(|| json!({}));
    let x_data_type = x.as_ref().map(data_type).unwrap_or("");
    let x_is_number = is_numerical_data_type(x_data_type);
    let x_is_float = x_data_type == "float";
    let x_is_date = is_date_type(x_data_type);
    let x_axis_mode = get_x_axis_mode(config);
    let is_x_discrete = x_axis_mode == "discrete";
    let y_placeholder = find_placeholder(placeholders, "y");
    let y_items = placeholder_items(placeholders, "y");
    let mut part = Map::new();
    let mut x_axis = Map::new();
    let mut y_axis = Map::new();
    let mut axes_formatting = json!({"xAxis": [], "yAxis": []});
    if let Some(formatting) =
        x_placeholder.and_then(|placeholder| axis_chartkit_formatting(placeholder, visualization_id))
    {
        axes_formatting["xAxis"] = Value::Array(vec![formatting]);
    }
    let mut exporting = json!({"csv": {"custom": {}, "columnHeaderFormatter": "wizard-export-column-names-formatter"}});
    if let Some(x) = &x {
        exporting["csv"]["custom"]["categoryHeader"] = get_field_exporting_options(x);
    }
    if !y_items.is_empty() {
        if y_items.iter().any(|item| data_type(item) == "float") {
            // Precision override is handled by `config_override`.
        }
        part.insert("plotOptions".to_string(), json!({}));
        let is_last_layer = match last_layer_id(config) {
            Some(last) => last == layer_id_of(placeholders, config),
            None => true,
        };
        let layer_y_placeholder = if is_last_layer { y_placeholder } else { None };
        if let Some(placeholder) = layer_y_placeholder {
            if let Some(formatting) = axis_chartkit_formatting(placeholder, visualization_id) {
                axes_formatting["yAxis"] = Value::Array(vec![formatting]);
            }
        }
        add_axis_formatter(&mut y_axis, layer_y_placeholder, None);
        if x_is_date || x_is_number {
            if let Some(x) = &x {
                let reversed = is_x_axis_reversed(x, array(config, "sort"), layer_id_of_string(config));
                if !reversed.is_null() {
                    x_axis.insert("reversed".to_string(), reversed);
                }
            }
            let not_logarithmic = s(&x_settings, "type") != "logarithmic";
            if is_x_discrete && not_logarithmic {
                x_axis.insert("type".to_string(), json!("category"));
            } else if x_is_date {
                x_axis.insert("type".to_string(), json!("datetime"));
            }
            let wizard_x_axis_formatter = if is_x_discrete && not_logarithmic && x_is_date {
                Some("wizard-x-axis-formatter")
            } else {
                None
            };
            add_axis_formatter(&mut x_axis, x_placeholder, wizard_x_axis_formatter);
        }
        if s(&extra, "tooltipSum").is_empty() || s(&extra, "tooltipSum") == "on" {
            part.insert("enableSum".to_string(), json!(true));
        }
        if let Some(x) = &x {
            if !is_measure_name_or_value(x) {
                part.insert("tooltipHeaderFormatter".to_string(), json!(fake_title(x)));
            }
        }
        if is_navigator_enabled(config) {
            x_axis.insert("ordinal".to_string(), json!(is_x_discrete));
        }
    } else if x_is_date || x_is_number {
        let _ = x_is_float;
    }
    if let Some(x) = &x {
        if is_html_field(x) {
            let labels = x_axis.entry("labels".to_string()).or_insert_with(|| json!({}));
            labels["useHTML"] = json!(true);
        }
    }
    part.insert("xAxis".to_string(), Value::Object(x_axis));
    part.insert("yAxis".to_string(), Value::Object(y_axis));
    part.insert("axesFormatting".to_string(), axes_formatting);
    part.insert("exporting".to_string(), exporting);
    let _ = graphs;
    Ok(part)
}

fn layer_id_of(placeholders: &[Value], config: &Value) -> String {
    let first_guid = placeholders
        .iter()
        .flat_map(|placeholder| array(placeholder, "items"))
        .next()
        .map(|item| json!(s(item, "guid")))
        .unwrap_or(Value::Null);
    if let Some(layers) = config.pointer("/visualization/layers").and_then(Value::as_array) {
        for layer in layers {
            let items = array(layer, "placeholders")
                .iter()
                .flat_map(|placeholder| array(placeholder, "items"))
                .any(|item| json!(s(item, "guid")) == first_guid);
            if items {
                return s(layer, "id").to_string();
            }
        }
    }
    config.pointer("/visualization/id").and_then(Value::as_str).unwrap_or("").to_string()
}

fn layer_id_of_string(config: &Value) -> &str {
    config.pointer("/visualization/id").and_then(Value::as_str).unwrap_or("")
}

/// Full `highchartsConfig`: base + overlay (`Object.assign` per layer) + `lodash.merge`.
pub fn build_library(config: &Value, graphs: &Value) -> Result<Value, String> {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let id = s(&visualization, "id").to_string();
    let layers = array(&visualization, "layers").to_vec();
    let layer_placeholders: Vec<Vec<Value>> = if layers.is_empty() {
        vec![array(&visualization, "placeholders").to_vec()]
    } else {
        layers.iter().map(|layer| array(layer, "placeholders").to_vec()).collect()
    };
    let base = build_highcharts_config(config);
    let mut overlay = Map::new();
    for (index, placeholders) in layer_placeholders.iter().enumerate() {
        let layer_id = if layers.is_empty() {
            id.clone()
        } else {
            layers.get(index).map(|layer| s(layer, "id").to_string()).unwrap_or_else(|| id.clone())
        };
        // An empty layer (no `y` and no `y2` series) of a combined chart contributes nothing:
        // skip it instead of failing the whole chart.
        let has_series = ["y", "y2"].iter().any(|placeholder_id| {
            find_placeholder(placeholders, placeholder_id)
                .map(|placeholder| !array(placeholder, "items").is_empty())
                .unwrap_or(false)
        });
        if !has_series {
            continue;
        }
        let part = match layer_id.as_str() {
            "line" | "area" | "area100p" => line_preparer_overlay(config, &layer_id, placeholders, graphs)?,
            "column" | "column100p" => bar_x_preparer_overlay(config, placeholders, graphs)?,
            _ => continue,
        };
        for (key, value) in part {
            overlay.insert(key, value);
        }
    }
    let overlay = Value::Object(overlay);
    Ok(merge_library_value(base, overlay))
}

// ---------------------------------------------------------------------------------------------
// data-api requests (`url/build-sources.js`, `url/build-request-body/*`)
// ---------------------------------------------------------------------------------------------

pub struct DatasetInfo {
    /// The `/fields` payload of the dataset, shared by every chart of the request instead of
    /// deep-cloned per chart.
    pub fields: std::sync::Arc<Value>,
    pub revision: std::sync::Arc<Value>,
}

pub struct GraphSourceRequest {
    pub key: String,
    pub dataset: String,
    pub body: Value,
}

fn is_raw_param_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !(items.len() == 1 && items[0].as_str() == Some("")),
        _ => true,
    }
}

fn is_param_valid(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => {
            !items.is_empty() && !items.iter().any(Value::is_null)
                && !(items.len() == 1 && items[0].as_str() == Some(""))
        }
        _ => true,
    }
}

/// Resolve `__interval_*` values the same way `__dlHelpers.resolveParams` does.
pub fn resolve_params(params: &Value) -> Value {
    // `resolveParams` (`shared/modules/config/wizard*` / `url/build-sources.js`): resolves
    // `__relative_*` dates and `__interval_<from>_<to>` markers (where either side may be a
    // relative date) into absolute interval strings. The v5.2 no-JS pipeline resolves params
    // only for control/table/pivot requests, so chart paths must resolve them here. The full
    // resolver (`resolveRelativeDate` / `resolveIntervalDate` port) lives in `pivot_engine`.
    match crate::pivot_engine::resolve_params(params) {
        Ok(resolved) => resolved,
        Err(_) => params.clone(),
    }
}

/// `prepareFilterValuesWithOperations` for the interval form used by the dashboard params.
/// The values come from `resolve_params`, but a marker that the resolver could not expand
/// (e.g. a malformed relative date) still must not reach the data-api as a `BETWEEN`.
fn param_filter_values(values: &[Value]) -> (Vec<Value>, String) {
    crate::pivot_engine::resolve_filter_values(values, "IN")
}

fn parameter_guids(schema: &[Value]) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    for field in schema {
        if is_parameter(field) {
            map.insert(s(field, "guid").to_string(), true);
            map.insert(s(field, "title").to_string(), true);
        }
    }
    map
}

fn split_params(params: &Value, schema: &[Value]) -> (Vec<(String, Vec<Value>)>, Vec<(String, Vec<Value>)>) {
    let parameters = parameter_guids(schema);
    let mut filters = Vec::new();
    let mut parameter_params = Vec::new();
    if let Some(object) = params.as_object() {
        for (key, value) in object {
            let values = match value {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            if parameters.contains_key(key) {
                parameter_params.push((key.clone(), values));
            } else {
                filters.push((key.clone(), values));
            }
        }
    }
    (filters, parameter_params)
}

fn field_in_dataset(field: &Value, dataset: &str) -> bool {
    match field.get("datasetId") {
        Some(Value::String(value)) => value == dataset,
        _ => true,
    }
}

/// `links[].fields[datasetId].field.guid`.
fn link_field_guid<'a>(link: &'a Value, dataset: &str) -> Option<&'a str> {
    link.get("fields")?
        .get(dataset)?
        .get("field")?
        .get("guid")?
        .as_str()
}

/// `getItemLinkWithDatasets` (`shared/utils/visualization-check.ts:18`): the guid of the
/// same linked field on `target_dataset`, when a link joins `item_dataset` to it.
fn linked_guid(
    links: &[Value],
    item_guid: &str,
    item_dataset: &str,
    target_dataset: &str,
) -> Option<String> {
    links.iter().find_map(|link| {
        if link_field_guid(link, item_dataset)? != item_guid {
            return None;
        }
        link_field_guid(link, target_dataset).map(str::to_string)
    })
}

/// `prepareFieldsForPayload` (`url/helpers/fields.ts`): a field belongs to the request when
/// it lives in the dataset or is joined into it through `links`.
fn field_usable_in_dataset(field: &Value, dataset: &str, links: &[Value]) -> bool {
    if field_in_dataset(field, dataset) {
        return true;
    }
    let guid = s(field, "guid");
    links.iter().any(|link| {
        link_field_guid(link, dataset).is_some()
            && link
                .get("fields")
                .and_then(Value::as_object)
                .map(|fields| {
                    fields
                        .values()
                        .any(|entry| entry.pointer("/field/guid").and_then(Value::as_str) == Some(guid))
                })
                .unwrap_or(false)
    })
}

/// A request `template` column (created by `buildTotalRequestFields` for totals blocks) has
/// no field behind it; it must never add a column. Response-side template fields are dropped
/// in `merge_responses`.
fn is_template_field(field: &Value) -> bool {
    field.pointer("/role_spec/role").and_then(Value::as_str) == Some("template")
}

fn build_request_body(
    config: &Value,
    visualization: &Value,
    placeholders: &[Value],
    chart_filters: &[Value],
    common_colors: &[Value],
    common_labels: &[Value],
    common_tooltips: &[Value],
    common_shapes: &[Value],
    common_segments: &[Value],
    sort: &[Value],
    links: &[Value],
    dataset: &str,
    dataset_schema: &[Value],
    revision: &Value,
    params: &Value,
) -> Result<Value, String> {
    let items = all_placeholder_items(placeholders);
    let fields: Vec<Value> = items.iter().filter(|item| !is_parameter(item)).cloned().collect();
    let item_parameters: Vec<Value> = items.iter().filter(|item| is_parameter(item)).cloned().collect();
    let mut all_items: Vec<Value> = items.clone();
    all_items.extend(common_colors.iter().cloned());
    all_items.extend(sort.iter().cloned());
    all_items.extend(common_labels.iter().cloned());
    all_items.extend(common_tooltips.iter().cloned());
    all_items.extend(common_shapes.iter().cloned());
    all_items.extend(common_segments.iter().cloned());
    let mut all_measures: HashMap<String, bool> = HashMap::new();
    for item in &all_items {
        if is_measure_field(item) {
            all_measures.insert(s(item, "guid").to_string(), true);
            all_measures.insert(s(item, "title").to_string(), true);
        }
    }
    let mut columns: Vec<String> = Vec::new();
    let push_column = |columns: &mut Vec<String>, guid: &str| {
        if !guid.is_empty() && !columns.iter().any(|existing| existing == guid) {
            columns.push(guid.to_string());
        }
    };
    // `prepareColumns` (`url/helpers/fields.ts:62`) calls `prepareFieldsForPayload` without
    // `links`: only fields of the current dataset become columns here; the join keys of the
    // other datasets are appended by the link loop below.
    for field in &fields {
        if is_template_field(field) {
            continue;
        }
        if field_in_dataset(field, dataset) {
            push_column(&mut columns, s(field, "guid"));
        }
    }
    for field in &item_parameters {
        if is_template_field(field) || !field_in_dataset(field, dataset) {
            continue;
        }
        push_column(&mut columns, s(field, "guid"));
    }
    for container in [common_colors, common_labels, common_tooltips, common_shapes, common_segments] {
        for item in container {
            if field_in_dataset(item, dataset) {
                if is_parameter(item) || is_template_field(item) {
                    continue;
                }
                push_column(&mut columns, s(item, "guid"));
            }
        }
    }
    // `build-request-body/index.ts:501`: every link that joins the current dataset to a field
    // used by the visualization contributes its own join key, otherwise the join cannot be
    // built from the answer.
    for link in links {
        let Some(field_guid) = link_field_guid(link, dataset) else { continue };
        let used = all_items.iter().any(|item| {
            link.get("fields")
                .and_then(Value::as_object)
                .and_then(|fields| fields.get(s(item, "datasetId")))
                .and_then(|entry| entry.pointer("/field/guid"))
                .and_then(Value::as_str)
                == Some(s(item, "guid"))
        });
        if used {
            push_column(&mut columns, field_guid);
        }
    }
    // order_by
    let mut order_by = Vec::new();
    for item in sort {
        let item_dataset = s(item, "datasetId");
        let guid = if item_dataset.is_empty() || item_dataset == dataset {
            s(item, "guid").to_string()
        } else {
            // `build-request-body/index.ts:514`: a sort on a linked field of another dataset
            // becomes a sort on the local side of the link; an unlinked foreign sort is dropped.
            match linked_guid(links, s(item, "guid"), item_dataset, dataset) {
                Some(guid) => guid,
                None => continue,
            }
        };
        push_column(&mut columns, &guid);
        let raw_direction = s(item, "direction");
        let direction = if raw_direction.is_empty() {
            "desc".to_string()
        } else {
            raw_direction.to_lowercase()
        };
        order_by.push(json!({"ref": {"type": "id", "id": guid}, "direction": direction, "block_id": 0}));
    }
    // chart-level filters (`formatFilters`): chart filters first, params filters that share a
    // column are skipped.
    let mut chart_filter_values: Vec<(String, String, Vec<Value>)> = Vec::new();
    for filter in chart_filters {
        if filter.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let filter_dataset = s(filter, "datasetId");
        let mut guid = s(filter, "guid").to_string();
        if !filter_dataset.is_empty() && filter_dataset != dataset {
            // A filter on another dataset is applied to this request only through a link
            // (`build-request-body/index.ts:297,352`).
            let linked = links.iter().any(|link| {
                link_field_guid(link, filter_dataset).is_some() && link_field_guid(link, dataset).is_some()
            });
            if !linked {
                continue;
            }
            if let Some(target) = linked_guid(links, &guid, filter_dataset, dataset) {
                guid = target;
            }
        }
        let Some(spec) = filter.get("filter") else { continue };
        let operation = {
            let operation_spec = spec.get("operation").cloned().unwrap_or(Value::Null);
            s(&operation_spec, "code").to_string()
        };
        let value = spec.get("value").cloned().unwrap_or(Value::Null);
        if value.is_null() || (value.as_str() == Some("")) {
            continue;
        }
        let values: Vec<Value> = match value {
            Value::Array(items) => items,
            other => vec![other],
        };
        // A saved chart filter stores a dashboard date range as one `__interval_*` string
        // (both sides may be `__relative_*`); the data-api only understands the resolved
        // pair for `BETWEEN`. Resolve it here, exactly like the JS engine does, instead of
        // forwarding the marker and getting `400 ... expects exactly two values`.
        let (values, operation) = crate::pivot_engine::resolve_filter_values(&values, &operation);
        let operation = if operation.is_empty() { "IN".to_string() } else { operation };
        if values.is_empty() {
            continue;
        }
        chart_filter_values.push((guid, operation, values));
    }
    // filters from params
    let (filter_params, parameter_params) = split_params(params, dataset_schema);
    let mut base_filters: Vec<Value> = Vec::new();
    for (key, values) in &filter_params {
        if !is_raw_param_valid(&Value::Array(values.clone())) || !is_param_valid(&Value::Array(values.clone())) {
            continue;
        }
        let field = dataset_schema
            .iter()
            .find(|field| s(field, "guid") == key.as_str() || s(field, "title") == key.as_str());
        let (values, operation) = param_filter_values(values);
        let mut guid = key.clone();
        // `formatParamsFilters` (`url/build-request-body/index.ts:110`): a dashboard param may
        // arrive with the title/guid of a linked field of another dataset; the request must use
        // the local side of the link.
        if !links.is_empty()
            && !dataset_schema.iter().any(|field| s(field, "guid") == guid)
        {
            let target = links.iter().find_map(|link| {
                let fields = link.get("fields")?.as_object()?;
                let matches = fields.values().any(|entry| {
                    entry.pointer("/field/guid").and_then(Value::as_str) == Some(guid.as_str())
                });
                if !matches {
                    return None;
                }
                link_field_guid(link, dataset).map(str::to_string)
            });
            if let Some(target) = target {
                guid = target;
            }
        }
        if guid.len() != 36 {
            if let Some(field) = field {
                let field_guid = s(field, "guid");
                if !field_guid.is_empty() {
                    guid = field_guid.to_string();
                }
            }
        }
        base_filters.push(json!({"ref": {"type": "id", "id": guid}, "operation": operation, "values": values}));
    }
    // `getMergedChartAndParamsFilters`: chart filters whose column has a param filter are dropped.
    let param_columns: Vec<String> = base_filters
        .iter()
        .filter_map(|filter| filter.pointer("/ref/id").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut chart_request_filters: Vec<Value> = Vec::new();
    for (guid, operation, values) in chart_filter_values {
        if param_columns.iter().any(|column| column == &guid) {
            continue;
        }
        let operation: &str = if operation.is_empty() { "EQ" } else { operation.as_str() };
        chart_request_filters.push(json!({
            "ref": {"type": "id", "id": guid},
            "operation": operation,
            "values": values,
        }));
    }
    let mut filters = chart_request_filters;
    for filter in base_filters {
        filters.push(filter);
    }

    // parameter_values
    let mut parameter_values = Vec::new();
    for (key, values) in &parameter_params {
        // `isParamValid`: an unset control (empty string / empty list) is not a parameter
        // value. Sending it as `""` would override the dataset parameter's `default_value`
        // and compile `DATETRUNC([Order date], '')` -> `ERROR: unit "" not recognized for
        // type timestamp with time zone`. Omitting it lets the data-api use the default.
        if !is_param_valid(&Value::Array(values.clone())) {
            continue;
        }
        let value = match values.first() {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => continue,
        };
        parameter_values.push(json!({"ref": {"type": "id", "id": key}, "value": value}));
    }
    let _ = item_parameters;
    // updates
    let updates: Vec<Value> = config
        .get("updates")
        .and_then(Value::as_array)
        .map(|updates| {
            updates
                .iter()
                .filter(|update| {
                    update
                        .pointer("/field/datasetId")
                        .and_then(Value::as_str)
                        .map(|id| id == dataset)
                        .unwrap_or(true)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let request_fields: Vec<Value> = columns
        .iter()
        .map(|guid| {
            let measure = all_measures.contains_key(guid);
            if measure {
                json!({"ref": {"type": "id", "id": guid}, "block_id": 0, "role_spec": {"role": "measure"}})
            } else {
                json!({"ref": {"type": "id", "id": guid}, "block_id": 0})
            }
        })
        .collect();
    let mut body = Map::new();
    body.insert("fields".to_string(), Value::Array(request_fields));
    body.insert("filters".to_string(), Value::Array(filters));
    body.insert("order_by".to_string(), Value::Array(order_by));
    body.insert("parameter_values".to_string(), Value::Array(parameter_values));
    body.insert("dataset_revision_id".to_string(), revision.clone());
    if !updates.is_empty() {
        body.insert("updates".to_string(), Value::Array(updates));
    }
    body.insert("ignore_nonexistent_filters".to_string(), json!(true));
    body.insert("with_totals".to_string(), json!(false));
    body.insert("autofill_legend".to_string(), json!(true));
    let _ = visualization;
    Ok(Value::Object(body))
}

pub struct GraphPrepared {
    pub config: Value,
    pub params: Value,
    pub requests: Vec<GraphSourceRequest>,
    /// `shared.sharedData.drillDownData` produced by `preprocessHierarchies` (null if absent).
    pub drill_down: Value,
}

/// Build the migrated config and every per-layer data-api request.
pub fn prepare(
    shared: &Value,
    params: &Value,
    datasets: &HashMap<String, DatasetInfo>,
) -> Result<GraphPrepared, String> {
    let config = server_config(shared)?;
    let params = resolve_params(params);
    let (config, drill_down) = preprocess_hierarchies(config, &params)?;
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let datasets_ids: Vec<String> = array(&config, "datasetsIds")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if datasets_ids.is_empty() {
        return Err("graph: the config has no datasets".to_string());
    }
    let links = array(&config, "links").to_vec();
    let layers = array(&visualization, "layers").to_vec();
    let mut requests = Vec::new();
    if layers.is_empty() {
        let placeholders = array(&visualization, "placeholders").to_vec();
        push_dataset_requests(
            &config,
            &visualization,
            &placeholders,
            array(&config, "filters"),
            array(&config, "colors"),
            array(&config, "labels"),
            array(&config, "tooltips"),
            array(&config, "shapes"),
            array(&config, "segments"),
            array(&config, "sort"),
            &links,
            &datasets_ids,
            datasets,
            &params,
            "",
            &mut requests,
        )?;
    } else {
        for layer in &layers {
            let placeholders = array(layer, "placeholders").to_vec();
            let common = layer.get("commonPlaceholders").cloned().unwrap_or_else(|| json!({}));
            let layer_id = layer
                .pointer("/layerSettings/id")
                .and_then(Value::as_str)
                .ok_or_else(|| "graph: layer id is missing".to_string())?;
            let sort = if !array(&common, "sort").is_empty() {
                array(&common, "sort").to_vec()
            } else {
                array(&config, "sort").to_vec()
            };
            let common_filters: Vec<Value> = array(&common, "filters")
                .iter()
                .cloned()
                .chain(array(&config, "filters").iter().cloned())
                .collect();
            push_dataset_requests(
                &config,
                &visualization,
                &placeholders,
                &common_filters,
                array(&common, "colors"),
                array(&common, "labels"),
                array(&common, "tooltips"),
                array(&common, "shapes"),
                array(&common, "segments"),
                &sort,
                &links,
                &datasets_ids,
                datasets,
                &params,
                layer_id,
                &mut requests,
            )?;
        }
    }
    if requests.is_empty() {
        return Err("graph: no placeholder field belongs to a dataset of the config".to_string());
    }
    Ok(GraphPrepared { config, params, requests, drill_down })
}

/// `build-sources.ts:prepareSingleSourceRequest`: one request per dataset that owns at least one
/// placeholder field (`prepareFieldsForPayload`; the layer branch also follows `links`).
#[allow(clippy::too_many_arguments)]
fn push_dataset_requests(
    config: &Value,
    visualization: &Value,
    placeholders: &[Value],
    chart_filters: &[Value],
    colors: &[Value],
    labels: &[Value],
    tooltips: &[Value],
    shapes: &[Value],
    segments: &[Value],
    sort: &[Value],
    links: &[Value],
    datasets_ids: &[String],
    datasets: &HashMap<String, DatasetInfo>,
    params: &Value,
    layer_id: &str,
    requests: &mut Vec<GraphSourceRequest>,
) -> Result<(), String> {
    let items = all_placeholder_items(placeholders);
    let single_dataset = datasets_ids.len() == 1;
    for dataset in datasets_ids {
        // `prepareFieldsForPayload` compares `field.datasetId` strictly; a config from before
        // `datasetId` existed only ever has one dataset, so a missing id means "this dataset".
        let has_fields = items.iter().any(|item| {
            match item.get("datasetId").and_then(Value::as_str) {
                Some(item_dataset) => item_dataset == dataset,
                None => single_dataset,
            }
        });
        if !has_fields {
            continue;
        }
        let Some(dataset_info) = datasets.get(dataset) else {
            return Err(format!("graph: dataset {dataset} metadata is missing"));
        };
        let dataset_index = array(config, "datasetsIds")
            .iter()
            .position(|value| value.as_str() == Some(dataset.as_str()))
            .unwrap_or(0);
        let local_fields = config
            .get("datasetsPartialFields")
            .and_then(Value::as_array)
            .and_then(|datasets| datasets.get(dataset_index))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut dataset_schema: Vec<Value> = dataset_info
            .fields
            .as_array()
            .cloned()
            .unwrap_or_default();
        dataset_schema.extend(local_fields);
        let body = build_request_body(
            config,
            visualization,
            placeholders,
            chart_filters,
            colors,
            labels,
            tooltips,
            shapes,
            segments,
            sort,
            links,
            dataset,
            &dataset_schema,
            &dataset_info.revision,
            params,
        )?;
        requests.push(GraphSourceRequest {
            key: format!("{dataset}_result{layer_id}"),
            dataset: dataset.clone(),
            body,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `data` building
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct GraphFetched {
    pub key: String,
    pub dataset: String,
    pub status: u16,
    pub latency: f64,
    pub size: u64,
    pub body: Value,
    pub data_export: Option<Value>,
}

/// The chart's `data` member: either a plain `Value` tree (chartkit/gravity bar/donut/tree)
/// or a typed, already-preparable builder output (gravity scatter, v9.5). The envelope writes
/// this enum straight into its body buffer, so the typed variant never materializes a second
/// JSON tree.
pub enum GraphData {
    Value(Value),
    Scatter(crate::gravity_scatter::ScatterData),
}

impl GraphData {
    pub fn value(value: Value) -> Self {
        Self::Value(value)
    }

    /// The `Value` shape. Allocates for the typed variant; used by tests and fallbacks.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Value(value) => value.clone(),
            Self::Scatter(data) => data.to_value(),
        }
    }

    pub fn is_object(&self) -> bool {
        match self {
            Self::Value(value) => value.is_object(),
            Self::Scatter(_) => true,
        }
    }

    /// Borrowed `Value` lookup; the typed variant has no tree to look into.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Value(value) => value.get(key),
            Self::Scatter(_) => None,
        }
    }
}

impl serde::Serialize for GraphData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(value) => value.serialize(serializer),
            Self::Scatter(data) => data.serialize(serializer),
        }
    }
}

pub struct GraphFinished {
    pub data: GraphData,
    pub config: Value,
    pub library: Value,
    pub sources: Value,
}

/// Value formatting helpers (`utils/misc-helpers.js:getFormatOptions`,
/// `getFormatOptionsFromFieldFormatting`).
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
        formatting["precision"] = json!(2);
    }
    formatting
}

fn format_options_from_field_formatting(formatting: &Value, data_type: &str) -> Value {
    let mut precision = 0;
    if data_type == "float" {
        precision = formatting
            .get("precision")
            .and_then(Value::as_f64)
            .map(|value| value as i64)
            .unwrap_or(2);
    }
    let empty = match formatting {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if empty {
        return json!({"chartKitFormatting": true, "chartKitPrecision": precision});
    }
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

fn series_title_formatter(value: &Value) -> String {
    value.as_str().unwrap_or("").to_string()
}

fn formatted_value(field: &Value, value: &Value) -> String {
    if value.is_null() {
        return "Null".to_string();
    }
    if field.get("formatting").is_some() && is_number_field(field) {
        // chartKitFormatNumberWrapper is not ported; identity string is enough for the charts in
        // scope, where only titles use it.
        return format_number_ru(value);
    }
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn format_number_ru(value: &Value) -> String {
    let number = number_value(value).unwrap_or(0.0);
    if (number - number.round()).abs() < f64::EPSILON {
        format!("{}", number.round() as i64)
    } else {
        format!("{}", number)
    }
}

fn get_label_value(value: Option<&Value>) -> String {
    match value {
        None => String::new(),
        Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `selectServerPalette` (`server/constants/index.js:46-62`).
fn select_server_palette(palette: &str, default_palette_id: &str) -> Vec<String> {
    let palettes = system_palettes();
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

fn get_colors_settings(
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
    } else if color_field.is_some() {
        // `getFieldUISettings` colors of the field; not used by the reference charts.
    }
    if colors.is_empty() {
        colors = select_server_palette(s(colors_config, "palette"), default_palette_id);
    }
    (mounted, colors)
}

fn mounted_color(mounted: &Map<String, Value>, colors: &[String], value: &str) -> Option<String> {
    let color = mounted.get(value)?.as_str()?.to_string();
    match color.parse::<usize>() {
        Ok(index) if !colors.is_empty() => Some(colors[index % colors.len()].clone()),
        _ => Some(color),
    }
}

fn get_color(index: usize, colors: &[String]) -> String {
    if colors.is_empty() {
        return String::new();
    }
    colors[index % colors.len()].clone()
}

fn map_and_colorize_graphs(
    graphs: &mut [Value],
    colors_config: &Value,
    is_colors_item_exists: bool,
    color_field: Option<&Value>,
    is_segments_exists: bool,
    used_colors: &mut Vec<String>,
    default_palette_id: &str,
) {
    let (mounted_colors, colors) = get_colors_settings(colors_config, color_field, default_palette_id);
    for (index, graph) in graphs.iter_mut().enumerate() {
        let color_value = graph.get("colorValue").and_then(Value::as_str).unwrap_or("").to_string();
        let shape_value = graph.get("shapeValue").and_then(Value::as_str).unwrap_or("").to_string();
        let color_title = {
            let key = graph.get("colorKey").and_then(Value::as_str).unwrap_or("");
            if !key.is_empty() {
                key.to_string()
            } else {
                let legend = graph.get("legendTitle").and_then(Value::as_str).unwrap_or("");
                if !legend.is_empty() {
                    legend.to_string()
                } else {
                    graph.get("name").and_then(Value::as_str).unwrap_or("").to_string()
                }
            }
        };
        let color_key = if (!color_value.is_empty() && color_value == color_title)
            || (!color_value.is_empty() && !shape_value.is_empty())
        {
            Some(color_value.clone())
        } else if is_colors_item_exists {
            Some(color_title.split(':').next().unwrap_or("").to_string())
        } else {
            Some(color_title.clone())
        };
        if let Some(color_key) = color_key.as_deref() {
            if !color_key.is_empty() {
                if let Some(color) = mounted_color(&mounted_colors, &colors, color_key) {
                    graph["color"] = json!(color);
                    continue;
                }
            }
        }
        let mut value = graph.get("colorValue").and_then(Value::as_str).unwrap_or("").to_string();
        if is_colors_item_exists && !shape_value.is_empty() {
            let legend = graph.get("legendTitle").and_then(Value::as_str).unwrap_or("");
            if !legend.is_empty() {
                value = legend.to_string();
            }
        }
        let color_index = {
            let position = used_colors.iter().position(|used| used == &value);
            match position {
                Some(position) => position,
                None => {
                    used_colors.push(value.clone());
                    used_colors.len() - 1
                }
            }
        };
        let _ = is_segments_exists;
        let _ = index;
        graph["color"] = json!(get_color(color_index, &colors));
    }
}

struct LayerContext<'a> {
    config: &'a Value,
    layer: &'a Value,
    placeholders: &'a [Value],
    common: &'a Value,
    used_colors: &'a mut Vec<String>,
    default_palette_id: &'a str,
}

fn graph_value_at(row: &Value, guid: &str, index_by_guid: &HashMap<String, usize>) -> Value {
    let Some(index) = index_by_guid.get(guid) else {
        return Value::Null;
    };
    row.get("data")
        .and_then(Value::as_array)
        .and_then(|cells| cells.get(*index))
        .cloned()
        .unwrap_or(Value::Null)
}

fn response_index_by_guid(response: &Value) -> HashMap<String, usize> {
    // The row legend defines the order of `row.data`; map each field guid to its data index.
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
            let guid = s(field, "id").to_string();
            index_by_guid.entry(guid).or_insert(position);
        }
    }
    index_by_guid
}

fn id_to_data_type(response: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(fields) = response.get("fields").and_then(Value::as_array) {
        for field in fields {
            map.entry(s(field, "id").to_string())
                .or_insert_with(|| data_type(field).to_string());
        }
    }
    map
}

// ---------------------------------------------------------------------------------------------
// `data` building: `prepare-line-data.js` / `prepare-bar-x.js`
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// `data` building: `prepare-line-data.js` / `prepare-bar-x.js`
// ---------------------------------------------------------------------------------------------

fn get_field_title(field: &Value) -> String {
    let fake = s(field, "fakeTitle");
    if !fake.is_empty() {
        return fake.to_string();
    }
    let original = s(field, "originalTitle");
    if !original.is_empty() {
        return original.to_string();
    }
    s(field, "title").to_string()
}

fn is_measure_value(field: &Value) -> bool {
    is_pseudo_field(field) && s(field, "title") == "Measure Values"
}

fn is_markup_field(field: &Value) -> bool {
    !s(field, "markupType").is_empty() || s(field, "data_type") == "markup"
}

fn number_to_js_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn get_distinct_value(value: &Value) -> String {
    if value.is_null() {
        "Null".to_string()
    } else {
        match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

/// `Intl.Collator(undefined, {numeric: true, sensitivity: "base"})` approximation: natural
/// (digit-run aware) comparison that ignores case.
fn collator_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let mut i = 0;
    let mut j = 0;
    while i < left_chars.len() && j < right_chars.len() {
        let a = left_chars[i];
        let b = right_chars[j];
        if a.is_ascii_digit() && b.is_ascii_digit() {
            let mut start_i = i;
            let mut start_j = j;
            while i < left_chars.len() && left_chars[i].is_ascii_digit() { i += 1; }
            while j < right_chars.len() && right_chars[j].is_ascii_digit() { j += 1; }
            let a_text: String = left_chars[start_i..i].iter().collect();
            let b_text: String = right_chars[start_j..j].iter().collect();
            let a_trim = a_text.trim_start_matches('0');
            let b_trim = b_text.trim_start_matches('0');
            let ordering = a_trim.len().cmp(&b_trim.len()).then_with(|| a_trim.cmp(b_trim));
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
            start_i = 0;
            start_j = 0;
        } else {
            let a_lower = a.to_lowercase().next().unwrap_or(a);
            let b_lower = b.to_lowercase().next().unwrap_or(b);
            if a_lower != b_lower {
                return a_lower.cmp(&b_lower);
            }
            i += 1;
            j += 1;
        }
    }
    left_chars.len().cmp(&right_chars.len())
}

fn group_integer(digits: &str) -> String {
    // Counter-based grouping: no `% 3` and no intermediate `Vec<char>` per formatted number.
    let mut first_group = digits.len();
    while first_group > 3 {
        first_group -= 3;
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let mut left = first_group;
    for ch in digits.chars() {
        if left == 0 {
            out.push('\u{a0}');
            left = 3;
        }
        out.push(ch);
        left -= 1;
    }
    out
}

/// `chartKitFormatNumberWrapper` (subset: number/percent, precision, grouping, prefix/postfix,
/// simple units). Used for `xFormatted` and formatted line titles.
fn format_number_chartkit(value: f64, options: &Value) -> String {
    let lang = s(options, "lang");
    let _ = lang;
    let format = {
        let format = s(options, "format");
        if format.is_empty() { "number".to_string() } else { format.to_string() }
    };
    let mut multiplier = options.get("multiplier").and_then(Value::as_f64).unwrap_or(1.0);
    let mut pre_postfix = String::new();
    if format == "percent" {
        multiplier = 100.0;
        pre_postfix = "%".to_string();
    }
    if s(options, "labelMode") == "percent" {
        pre_postfix = "%".to_string();
    }
    let prefix = s(options, "prefix");
    let postfix = s(options, "postfix");
    let unit = s(options, "unit");
    let precision = options.get("precision").cloned().unwrap_or(Value::Null);
    let mut unit_rate = options.get("unitRate").and_then(Value::as_i64);
    if unit_rate.is_none() {
        unit_rate = match unit {
            "" | "default" => Some(0),
            "k" => Some(1),
            "m" => Some(2),
            "b" => Some(3),
            "t" => Some(4),
            _ => None,
        };
    }
    let mut number = value * multiplier;
    let rate: i64 = match unit_rate {
        Some(rate) => rate,
        None => {
            // `auto`: scale down by 1000 while the integer part is >= 1000 (max 10 steps).
            let mut rate: i64 = 0;
            while rate < 10 && (number / 1000f64.powi(rate as i32)).abs() >= 1.0 {
                rate += 1;
            }
            rate.saturating_sub(1)
        }
    };
    number /= 1000f64.powi(rate.max(0) as i32);
    let precision_number = precision.as_f64().map(|value| value as i64);
    if let Some(precision) = precision_number {
        let factor = 10f64.powi(precision as i32);
        number = (number * factor).round() / factor;
    }
    let show_rank_delimiter = options
        .get("showRankDelimiter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let minimum_fraction = precision_number.unwrap_or(0).max(0);
    let mut formatted = format_grouped_number(number, show_rank_delimiter, minimum_fraction);
    let unit_text = match rate {
        0 => "",
        1 => "тыс.",
        2 => "млн",
        3 => "млрд",
        _ => "",
    };
    formatted.push_str(unit_text);
    format!("{prefix}{formatted}{pre_postfix}{postfix}")
}

fn format_grouped_number(number: f64, group: bool, minimum_fraction: i64) -> String {
    let negative = number.is_sign_negative();
    let absolute = number.abs();
    let mut text = format!("{absolute}");
    if text.contains('e') || text.contains('E') {
        // Very small/large numbers: approximate JS `Number#toString` fallback.
        text = format!("{absolute:.20}");
        while text.ends_with('0') && text.contains('.') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    let (integer_part, fraction_part) = match text.split_once('.') {
        Some((integer, fraction)) => (integer.to_string(), fraction.to_string()),
        None => (text.clone(), String::new()),
    };
    let mut fraction = fraction_part;
    let minimum = minimum_fraction.max(0) as usize;
    while fraction.len() < minimum {
        fraction.push('0');
    }
    while fraction.len() > minimum && fraction.ends_with('0') {
        fraction.pop();
    }
    let mut output = if group {
        group_integer(&integer_part)
    } else {
        integer_part
    };
    if !fraction.is_empty() {
        output.push(',');
        output.push_str(&fraction);
    }
    if negative {
        output.insert(0, '-');
    }
    output
}

fn chartkit_formatting_of(formatting: &Value, data_type: &str) -> Value {
    let empty = Value::Null;
    let value = if formatting.is_object() { formatting } else { &empty };
    format_options_from_field_formatting(value, data_type)
}

/// `getItemsValues` (`line/helpers/utils.js`).
fn items_values(item: &Value, _id_to_title: &HashMap<String, String>, row: &Value, index_by_guid: &HashMap<String, usize>) -> (Value, Value) {
    let value = graph_value_at(row, s(item, "guid"), index_by_guid);
    let formatted = if item.get("formatting").is_some() && is_numerical_data_type(data_type(item)) {
        let number = number_value(&value).unwrap_or(0.0);
        json!(format_number_chartkit(number, &get_format_options(item)))
    } else if value.is_null() {
        json!("Null")
    } else {
        value.clone()
    };
    (value, formatted)
}

struct LinePoint {
    value: Value,
    color_value: Option<Value>,
}

struct LineDraft {
    key: String,
    title: String,
    legend_title: Option<String>,
    formatted_name: Option<String>,
    drill_down_filter_value: Option<String>,
    color_key: Option<String>,
    color_value: Option<Value>,
    shape_value: Option<String>,
    stack: Option<Value>,
    data: HashMap<String, LinePoint>,
    labels: HashMap<String, Value>,
    field_title: String,
    point_conflict: bool,
    nulls_setting: Option<String>,
    segment_name_key: Option<String>,
    is_y2: bool,
}

impl LineDraft {
    fn new(key: String) -> Self {
        Self {
            key,
            title: String::new(),
            legend_title: None,
            formatted_name: None,
            drill_down_filter_value: None,
            color_key: None,
            color_value: None,
            shape_value: None,
            stack: None,
            data: HashMap::new(),
            labels: HashMap::new(),
            field_title: String::new(),
            point_conflict: false,
            nulls_setting: None,
            segment_name_key: None,
            is_y2: false,
        }
    }
}

/// `prepareLineData`/`prepareBarX` for one layer (`preparers/line/prepare-line-data.js`,
/// `preparers/bar-x/prepare-bar-x.js`).
fn build_layer_data(context: &mut LayerContext<'_>, response: &Value) -> Result<Value, String> {
    let config = context.config;
    let placeholders = context.placeholders;
    let common = context.common;
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let layer_id = s(context.layer, "id").to_string();
    let visualization_id = if visualizations_with_layers(&visualization) {
        layer_id.clone()
    } else {
        s(&visualization, "id").to_string()
    };
    let is_bar_x = matches!(visualization_id.as_str(), "column" | "column100p");
    if !matches!(
        visualization_id.as_str(),
        "line" | "area" | "area100p" | "column" | "column100p"
    ) {
        return Err(format!("graph: layer renderer {visualization_id} is not ported"));
    }
    let colors = array(common, "colors");
    let shapes = array(common, "shapes");
    let segments = array(common, "segments");
    let labels = array(common, "labels");
    let colors_config = common.get("colorsConfig").cloned().unwrap_or_else(|| json!({}));
    let sort = if !array(common, "sort").is_empty() {
        array(common, "sort")
    } else {
        array(config, "sort")
    };
    let extra = config.get("extraSettings").cloned().unwrap_or_else(|| json!({}));

    let x_placeholder = find_placeholder(placeholders, "x")
        .ok_or_else(|| "graph: x placeholder is missing".to_string())?;
    let x = array(x_placeholder, "items").first().cloned();
    let y_placeholder = find_placeholder(placeholders, "y")
        .ok_or_else(|| "graph: y placeholder is missing".to_string())?;
    let mut y_items: Vec<Value> = array(y_placeholder, "items").to_vec();
    let mut y2_items: Vec<Value> = placeholder_items(placeholders, "y2").to_vec();
    // Combined charts put secondary-axis series in `y2`; a line/area layer may legitimately
    // have only `y2` filled (the `area`/`line` preparers merge both sections, so that layer
    // renders the secondary series). The bar-x preparer reads only `y`, so a `column` layer
    // with its series in `y2` gets them promoted instead of failing the whole chart.
    if is_bar_x && y_items.is_empty() && !y2_items.is_empty() {
        y_items = std::mem::take(&mut y2_items);
    }
    if y_items.is_empty() && y2_items.is_empty() {
        return Err("graph: y placeholder is empty".to_string());
    }
    let is_multi_axis = !y_items.is_empty() && !y2_items.is_empty();
    let x2 = if is_bar_x {
        array(x_placeholder, "items").get(1).cloned()
    } else {
        None
    };
    let x2_is_date = x2
        .as_ref()
        .map(|field| id_to_data_type(response).get(s(field, "guid")).map(|dtype| is_date_type(dtype)).unwrap_or(false))
        .unwrap_or(false);
    let x2_data_type = x2
        .as_ref()
        .and_then(|field| id_to_data_type(response).get(s(field, "guid")).cloned())
        .unwrap_or_default();
    let color_item = colors.first().cloned();
    let shape_item = shapes.first().cloned();
    let segment_field = segments.first().cloned();
    let label_item = labels.first().cloned();
    if let Some(item) = &color_item {
        if is_markup_field(item) {
            return Err("graph: markup colours are not ported".to_string());
        }
    }
    if let Some(item) = &x {
        if is_markup_field(item) {
            return Err("graph: markup x fields are not ported".to_string());
        }
    }
    if let Some(item) = &label_item {
        if is_markup_field(item) {
            return Err("graph: markup labels are not ported".to_string());
        }
    }
    if let Some(item) = &segment_field {
        if is_markup_field(item) {
            return Err("graph: markup segments are not ported".to_string());
        }
    }
    if colors_config.get("colorMode").and_then(Value::as_str) == Some("gradient") && color_item.is_some() {
        return Err("graph: gradient colours are not ported".to_string());
    }
    let is_color_item_exist = color_item
        .as_ref()
        .map(|field| s(field, "type") != "PSEUDO")
        .unwrap_or(false);
    let is_shape_item_exist = shape_item
        .as_ref()
        .map(|field| s(field, "type") != "PSEUDO")
        .unwrap_or(false);
    let is_pseudo_color_exist = color_item.as_ref().map(is_pseudo_field).unwrap_or(false);
    let is_pseudo_shape_exist = shape_item.as_ref().map(is_pseudo_field).unwrap_or(false);
    let is_color_measure_names = color_item.as_ref().map(is_measure_name).unwrap_or(false);
    let is_color_measure_value = color_item.as_ref().map(is_measure_value).unwrap_or(false);
    let is_colorize_by_measure = color_item.as_ref().map(is_measure_field).unwrap_or(false);
    if is_colorize_by_measure {
        return Err("graph: colour by measure is not ported".to_string());
    }
    let color_mode_gradient = colors_config.get("colorMode").and_then(Value::as_str) == Some("gradient");
    let x_data_type = x
        .as_ref()
        .and_then(|field| id_to_data_type(response).get(s(field, "guid")).cloned())
        .unwrap_or_else(|| x.as_ref().map(data_type).unwrap_or("").to_string());
    let x_is_date = is_date_type(&x_data_type);
    let x_is_number = is_numerical_data_type(&x_data_type);
    let x_is_pseudo = x.as_ref().map(is_pseudo_field).unwrap_or(false);
    let x_axis_mode = get_x_axis_mode(config);
    let is_x_discrete = x_axis_mode == "discrete";
    let categories_seen: Vec<String> = Vec::new();
    let _ = categories_seen;
    let index_by_guid = response_index_by_guid(response);
    let data_types = id_to_data_type(response);
    let id_to_title: HashMap<String, String> = response
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .map(|field| (s(field, "id").to_string(), s(field, "title").to_string()))
                .collect()
        })
        .unwrap_or_default();
    // Borrow the rows instead of deep-cloning the full result set (the loops below only read).
    let no_rows: &[Value] = &[];
    let rows: &[Value] = response
        .pointer("/result_data/0/rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(no_rows);

    // segments map (`getSegmentMap`): sorted distinct segment names, each becomes a y axis.
    let segment_index_in_order = segment_field
        .as_ref()
        .and_then(|field| {
            let title = id_to_title
                .get(s(field, "guid"))
                .cloned()
                .unwrap_or_else(|| s(field, "title").to_string());
            let _ = title;
            find_index_in_order_local(response, field)
        });
    let mut segments_order: Vec<String> = Vec::new();
    if let Some(index) = segment_index_in_order {
        for row in rows {
            let name = graph_value_at(row, "", &index_by_guid);
            let _ = name;
            let values = row.get("data").and_then(Value::as_array);
            if let Some(values) = values {
                let name = values.get(index).cloned().unwrap_or(Value::Null);
                let name = if name.is_null() { "Null".to_string() } else if let Some(text) = name.as_str() { text.to_string() } else { name.to_string() };
                if !segments_order.contains(&name) {
                    segments_order.push(name);
                }
            }
        }
        if segment_field.as_ref().map(is_number_field).unwrap_or(false) {
            segments_order.sort_by(|a, b| collator_cmp(a, b));
        } else {
            segments_order.sort_by(|a, b| collator_cmp(a, b));
        }
    }
    let has_opposite_y_axis = !y2_items.is_empty();
    let mut segments_map: Vec<(String, i64, bool)> = Vec::new();
    for (index, name) in segments_order.iter().enumerate() {
        let segment_index = segments_map.len() as i64;
        segments_map.push((name.clone(), segment_index, false));
        if has_opposite_y_axis {
            segments_map.push((format!("{name}__y2"), segment_index + 1, true));
        }
        let _ = index;
    }
    let segments_map_by_name = |name: &str| -> Option<(i64, bool)> {
        segments_map
            .iter()
            .find(|(key, _, _)| key == name)
            .map(|(_, index, opposite)| (*index, *opposite))
    };

    // merged y sections: y + y2 (line/area only)
    let mut merged_y: Vec<(Value, bool, Option<String>)> = Vec::new();
    let nulls_y1 = y_placeholder
        .get("settings")
        .and_then(|settings| settings.get("nulls"))
        .cloned();
    let y2_placeholder = find_placeholder(placeholders, "y2");
    let nulls_y2 = y2_placeholder
        .and_then(|placeholder| placeholder.get("settings"))
        .and_then(|settings| settings.get("nulls"))
        .cloned();
    for item in &y_items {
        merged_y.push((item.clone(), true, nulls_y1.as_ref().and_then(Value::as_str).map(str::to_string)));
    }
    if !is_bar_x {
        for item in &y2_items {
            merged_y.push((item.clone(), false, nulls_y2.as_ref().and_then(Value::as_str).map(str::to_string)));
        }
    }
    let _labels_values: HashMap<String, HashMap<String, Value>> = HashMap::new();
    let mut lines: Vec<LineDraft> = Vec::new();
    let mut line_index: HashMap<String, usize> = HashMap::new();
    let mut categories: Vec<Value> = Vec::new();
    let mut categories_seen: Vec<String> = Vec::new();
    let mut categories2: Vec<Value> = Vec::new();
    let mut categories2_seen: Vec<String> = Vec::new();
    let series_name_formatter = |value: &Value| -> String {
        match value {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    };

    for row in rows {
        // x value
        let mut x_value = Value::Null;
        if let Some(x_field) = &x {
            let raw = graph_value_at(row, s(x_field, "guid"), &index_by_guid);
            if raw.is_null() && !x_is_pseudo {
                continue;
            }
            if x_is_pseudo {
                for item in &y_items {
                    let title = fake_title(item);
                    if !categories_seen.contains(&title) {
                        categories_seen.push(title.clone());
                        categories.push(json!(title));
                    }
                }
            } else {
                x_value = if is_number_field(x_field) || x_is_number {
                    json!(number_value(&raw).unwrap_or(0.0))
                } else if is_date_field(x_field) || x_is_date {
                    json!(date_axis_value(&raw, &x_data_type).unwrap_or(0))
                } else {
                    raw.clone()
                };
                let key = match &x_value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                if !categories_seen.contains(&key) {
                    categories_seen.push(key);
                    categories.push(x_value.clone());
                }
            }
        } else if y_items.len() > 1 {
            let value = "Measure Names".to_string();
            if !categories_seen.contains(&value) {
                categories_seen.push(value.clone());
                categories.push(json!(value));
            }
        } else if let Some(item) = y_items.first() {
            let value = fake_title(item);
            if !categories_seen.contains(&value) {
                categories_seen.push(value.clone());
                categories.push(json!(value));
            }
        }
        // x2 value
        let mut x2_value = Value::Null;
        if let Some(x2_field) = &x2 {
            let raw = graph_value_at(row, s(x2_field, "guid"), &index_by_guid);
            let is_x2_pseudo = is_pseudo_field(x2_field);
            if raw.is_null() && !is_x2_pseudo {
                continue;
            }
            if !is_x2_pseudo {
                x2_value = if is_number_field(x2_field) {
                    json!(number_value(&raw).unwrap_or(0.0))
                } else if is_date_field(x2_field) {
                    json!(date_axis_value(&raw, &x2_data_type).unwrap_or(0))
                } else {
                    raw.clone()
                };
                let key = match &x2_value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                if !categories2_seen.contains(&key) {
                    categories2_seen.push(key);
                    categories2.push(x2_value.clone());
                }
            }
        }
        // prepareLines for every merged y section
        for (item, is_first_section, nulls_setting) in &merged_y {
            let guid = s(item, "guid");
            let y_title = id_to_title
                .get(guid)
                .cloned()
                .unwrap_or_else(|| s(item, "title").to_string());
            let shown_title = {
                let fake = s(item, "fakeTitle");
                if !fake.is_empty() { fake.to_string() } else { y_title.clone() }
            };
            let y_data_type = data_types
                .get(guid)
                .cloned()
                .unwrap_or_else(|| data_type(item).to_string());
            let raw_y = graph_value_at(row, guid, &index_by_guid);
            let y_value = if is_numerical_data_type(&y_data_type) {
                if raw_y.is_null() {
                    if nulls_setting.as_deref() == Some("as-0") { json!(0) } else { Value::Null }
                } else {
                    json!(number_value(&raw_y).unwrap_or(0.0))
                }
            } else if is_date_type(&y_data_type) && !raw_y.is_null() {
                json!(date_axis_value(&raw_y, &y_data_type).unwrap_or(0))
            } else {
                raw_y.clone()
            };
            let y_item_formatting = get_format_options(item);
            let _series_options = chartkit_formatting_of(&y_item_formatting, &y_data_type);
            let segment_name_key = if segments_order.is_empty() {
                None
            } else {
                let index = segment_index_in_order.unwrap_or(0);
                let values = row.get("data").and_then(Value::as_array);
                let name = values
                    .and_then(|values| values.get(index))
                    .cloned()
                    .unwrap_or(Value::Null);
                let name = if name.is_null() { "Null".to_string() } else if let Some(text) = name.as_str() { text.to_string() } else { name.to_string() };
                Some(if *is_first_section { name } else { format!("{name}__y2") })
            };
            // line key / properties
            let mut key;
            let mut title;
            let mut legend_title: Option<String> = None;
            let mut formatted_name: Option<String> = None;
            let mut drill_down_filter_value: Option<String> = None;
            let mut color_key: Option<String> = None;
            let mut color_value: Option<Value> = None;
            let mut shape_value: Option<String> = None;
            let mut stack: Option<Value> = None;
            if is_color_measure_value || is_colorize_by_measure {
                let parsed = if is_color_measure_value {
                    number_value(&y_value)
                } else {
                    let color_index = find_index_in_order_local(response, color_item.as_ref().unwrap());
                    color_index
                        .and_then(|index| row.get("data").and_then(Value::as_array).and_then(|values| values.get(index)).and_then(number_value))
                };
                key = shown_title.clone();
                if stack.is_none() && x2.is_some() {
                    stack = Some(x2_value.clone());
                }
                let _ = parsed;
                if x2.is_some() {
                    title = format!("{}: {}", shown_title, x2_value);
                    legend_title = Some(shown_title.clone());
                    formatted_name = Some(format!("{}: {}", shown_title, x2_value));
                } else {
                    title = shown_title.clone();
                    formatted_name = Some(shown_title.clone());
                }
                color_key = Some("colorValue".to_string());
                if let Some(value) = parsed {
                    color_value = Some(json!(value));
                }
            } else if is_color_item_exist || is_shape_item_exist {
                let mut items: Vec<Value> = Vec::new();
                let mut is_items_equal = false;
                if is_color_item_exist && is_shape_item_exist {
                    is_items_equal = s(color_item.as_ref().unwrap(), "guid") == s(shape_item.as_ref().unwrap(), "guid");
                    items.push(color_item.clone().unwrap());
                    items.push(shape_item.clone().unwrap());
                } else if is_shape_item_exist {
                    items.push(shape_item.clone().unwrap());
                } else {
                    items.push(color_item.clone().unwrap());
                }
                let mapped: Vec<(Value, Value)> = items
                    .iter()
                    .map(|item| items_values(item, &id_to_title, row, &index_by_guid))
                    .collect();
                let combined_value;
                let formatted_value;
                if is_items_equal {
                    let (value, formatted) = mapped[0].clone();
                    combined_value = if value.is_string() { Some(format!("{}{}{}", value.as_str().unwrap_or(""), "__COLOR_SHAPE_SEPARATOR__", value.as_str().unwrap_or(""))) } else { None };
                    formatted_value = formatted;
                } else {
                    let mut value_acc = String::new();
                    let mut formatted_acc = String::new();
                    let mut extra_acc = String::new();
                    for (index, (value, formatted)) in mapped.iter().enumerate() {
                        let mut distinct = get_distinct_value(value);
                        let mut formatted_text = match formatted {
                            Value::String(text) => text.clone(),
                            Value::Null => "Null".to_string(),
                            other => other.to_string(),
                        };
                        let mut extra = distinct.clone();
                        if index != mapped.len() - 1 {
                            distinct.push('-');
                            formatted_text.push('-');
                            extra.push_str("__COLOR_SHAPE_SEPARATOR__");
                        } else if !is_color_item_exist {
                            extra = format!("__COLOR_SHAPE_SEPARATOR__{extra}");
                        }
                        value_acc.push_str(&distinct);
                        formatted_acc.push_str(&formatted_text);
                        extra_acc.push_str(&extra);
                    }
                    combined_value = if extra_acc.is_empty() { None } else { Some(extra_acc) };
                    formatted_value = json!(formatted_acc);
                }
                let (color_part, shape_part) = match combined_value {
                    Some(combined) => match combined.split_once("__COLOR_SHAPE_SEPARATOR__") {
                        Some((color, shape)) => (color.to_string(), shape.to_string()),
                        None => (String::new(), String::new()),
                    },
                    None => (String::new(), String::new()),
                };
                let has_field = !color_part.is_empty() || !shape_part.is_empty();
                let field_title = |default: &str| -> String {
                    if has_field { s(item, "title").to_string() } else { default.to_string() }
                };
                let formatted_text = match &formatted_value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let color_value_string = if !color_part.is_empty() { color_part.clone() } else { field_title(&formatted_text) };
                let shape_value_string = if !shape_part.is_empty() { shape_part.clone() } else { field_title(&formatted_text) };
                // `itemValues.value`: distinct values joined with `-` (or the single value);
                // `extraValue` keeps the color/shape split markers.
                let mut item_values_value = String::new();
                let mut item_values_formatted = String::new();
                let mut item_values_extra = String::new();
                if is_items_equal {
                    let (value, formatted) = mapped[0].clone();
                    item_values_value = get_distinct_value(&value);
                    item_values_formatted = match &formatted { Value::String(text) => text.clone(), other => other.to_string() };
                    if value.is_string() {
                        item_values_extra = format!("{item_values_value}__COLOR_SHAPE_SEPARATOR__{item_values_value}");
                    }
                } else {
                    for (index, (value, formatted)) in mapped.iter().enumerate() {
                        let mut distinct = get_distinct_value(value);
                        let mut formatted_text = match formatted { Value::String(text) => text.clone(), Value::Null => "Null".to_string(), other => other.to_string() };
                        let mut extra = distinct.clone();
                        if index != mapped.len() - 1 {
                            distinct.push('-');
                            formatted_text.push('-');
                            extra.push_str("__COLOR_SHAPE_SEPARATOR__");
                        } else if !is_color_item_exist {
                            extra = format!("__COLOR_SHAPE_SEPARATOR__{extra}");
                        }
                        item_values_value.push_str(&distinct);
                        item_values_formatted.push_str(&formatted_text);
                        item_values_extra.push_str(&extra);
                    }
                }
                let (split_color, split_shape) = match item_values_extra.split_once("__COLOR_SHAPE_SEPARATOR__") {
                    Some((color, shape)) => (color.to_string(), shape.to_string()),
                    None => (item_values_extra.clone(), String::new()),
                };
                let has_color_value = !split_color.is_empty();
                let has_shape_value = !split_shape.is_empty();
                let has_any = has_color_value || has_shape_value;
                let field_title_value = if has_any { get_field_title(item) } else { item_values_formatted.clone() };
                color_value = Some(json!(if has_color_value { split_color.clone() } else { field_title_value.clone() }));
                shape_value = Some(if has_shape_value { split_shape.clone() } else { field_title_value.clone() });
                let is_multi_line_with_shape = is_shape_item_exist && y_items.len() > 1;
                let item_value_text = if item_values_value.is_empty() { shown_title.clone() } else { item_values_value.clone() };
                key = if x2.is_some() {
                    let x2_key_text = if x2.as_ref().map(is_pseudo_field).unwrap_or(false) {
                        shown_title.clone()
                    } else {
                        match &x2_value {
                            Value::String(text) => text.clone(),
                            Value::Number(number) => number.as_f64().map(number_to_js_string).unwrap_or_default(),
                            other => other.to_string(),
                        }
                    };
                    format!("{item_value_text}-{x2_key_text}")
                } else if is_multi_axis || is_multi_line_with_shape {
                    format!("{item_value_text}-{shown_title}")
                } else {
                    item_value_text.clone()
                };
                if let Some(segment) = &segment_name_key {
                    key = format!("{key}__{segment}");
                }
                let formatted_text = item_values_formatted.clone();
                if x2.is_some() {
                    stack = Some(x2_value.clone());
                    let x2_is_pseudo_value = x2.as_ref().map(is_pseudo_field).unwrap_or(false);
                    let mut x2_text = if x2_is_pseudo_value {
                        shown_title.clone()
                    } else if x2_value.is_null() {
                        String::new()
                    } else {
                        match &x2_value {
                            Value::String(text) => text.clone(),
                            Value::Number(number) => number.as_f64().map(number_to_js_string).unwrap_or_default(),
                            other => other.to_string(),
                        }
                    };
                    if x2_is_date {
                        let format = x2.as_ref().and_then(|field| field.get("format")).and_then(Value::as_str).unwrap_or("");
                        let millis = x2_value.as_i64().unwrap_or(0);
                        let value_type = if x2_data_type.is_empty() { "genericdatetime" } else { x2_data_type.as_str() };
                        x2_text = format_date_ms(millis, if format.is_empty() { default_date_format(value_type) } else { format });
                    }
                    if formatted_text == x2_text {
                        title = formatted_text.clone();
                        formatted_name = Some(formatted_text.clone());
                    } else {
                        title = format!("{formatted_text}: {x2_text}");
                        legend_title = Some(formatted_text.clone());
                        formatted_name = Some(title.clone());
                        drill_down_filter_value = Some(item_values_value.clone());
                    }
                } else if is_multi_axis || is_multi_line_with_shape {
                    title = format!("{formatted_text}: {shown_title}");
                    formatted_name = Some(title.clone());
                    drill_down_filter_value = Some(item_values_value.clone());
                } else {
                    title = formatted_text.clone();
                    formatted_name = Some(formatted_text.clone());
                    drill_down_filter_value = Some(item_values_value.clone());
                    legend_title = Some(get_colored_line_legend_title(
                        config,
                        item,
                        color_item.as_ref().unwrap(),
                        &formatted_text,
                        &visualization,
                    ));
                }
                let _ = (color_value_string, shape_value_string);
            } else {
                // plain line(s) (`mapDataToLines`)
                key = shown_title.clone();
                if let Some(segment) = &segment_name_key {
                    key = format!("{key}__{segment}");
                }
                let field_title = {
                    let fake = s(item, "fakeTitle").to_string();
                    if !fake.is_empty() {
                        fake
                    } else if let Some(title) = id_to_title.get(guid) {
                        if !title.is_empty() { title.clone() } else { s(item, "title").to_string() }
                    } else {
                        s(item, "title").to_string()
                    }
                };
                color_key = Some(field_title.clone());
                if is_pseudo_color_exist {
                    let config_key = if is_color_measure_names {
                        Some(fake_title(item))
                    } else {
                        None
                    };
                    if let Some(config_key) = config_key {
                        color_key = Some(config_key.clone());
                        color_value = Some(json!(config_key));
                    } else {
                        color_value = Some(json!(shown_title));
                    }
                }
                if is_pseudo_shape_exist {
                    let mounted = config
                        .get("shapesConfig")
                        .and_then(|shapes_config| shapes_config.get("mountedShapes"))
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let color_and_shape_key = fake_title(item);
                    let mapped = mounted.get(&shown_title).cloned().unwrap_or(Value::Null);
                    let mapped_by_field = mounted.get(&color_and_shape_key).cloned().unwrap_or(Value::Null);
                    if !mapped.is_null() && mapped.as_str() != Some("auto") {
                        shape_value = Some(shown_title.clone());
                    } else if !mapped_by_field.is_null() && mapped_by_field.as_str() != Some("auto") {
                        shape_value = Some(color_and_shape_key);
                    }
                }
                if x2.is_some() {
                    stack = Some(x2_value.clone());
                    let x2_is_pseudo_value = x2.as_ref().map(is_pseudo_field).unwrap_or(false);
                    let x2_text = if x2_is_pseudo_value {
                        shown_title.clone()
                    } else if x2_value.is_null() {
                        String::new()
                    } else {
                        match &x2_value {
                            Value::String(text) => text.clone(),
                            Value::Number(number) => number.as_f64().map(number_to_js_string).unwrap_or_default(),
                            other => other.to_string(),
                        }
                    };
                    let formatted_text = shown_title.clone();
                    if formatted_text == x2_text {
                        title = formatted_text.clone();
                        legend_title = Some(formatted_text);
                    } else {
                        title = format!("{formatted_text}: {x2_text}");
                        legend_title = Some(format!("{formatted_text}: {x2_text}"));
                    }
                    stack = Some(if x2_is_pseudo_value { json!(x2_text.clone()) } else { x2_value.clone() });
                } else {
                    title = shown_title.clone();
                }
            }
            if !x2.is_some()
                && title.is_empty() {
                    title = shown_title.clone();
                }
            let line_index_entry = line_index.get(&key).copied();
            let index = match line_index_entry {
                Some(index) => index,
                None => {
                    line_index.insert(key.clone(), lines.len());
                    let mut line = LineDraft::new(key.clone());
                    line.title = title;
                    line.legend_title = legend_title;
                    line.formatted_name = formatted_name;
                    line.drill_down_filter_value = drill_down_filter_value;
                    line.color_key = color_key;
                    line.color_value = color_value;
                    line.shape_value = shape_value;
                    line.stack = stack;
                    line.field_title = fake_title(item);
                    line.nulls_setting = nulls_setting.clone();
                    line.segment_name_key = segment_name_key.clone();
                    line.is_y2 = !*is_first_section;
                    lines.push(line);
                    lines.len() - 1
                }
            };
            let category_key = match &x_value {
                Value::String(text) => text.clone(),
                Value::Null => shown_title.clone(),
                other => other.to_string(),
            };
            let existing = lines[index].data.get(&category_key).is_some();
            if existing {
                lines[index].point_conflict = true;
            }
            let line_color_value = if color_mode_gradient {
                lines[index].color_value.clone()
            } else {
                None
            };
            lines[index].data.insert(
                category_key.clone(),
                LinePoint {
                    value: y_value.clone(),
                    color_value: line_color_value,
                },
            );
            // label value (`mergeLabelDataWithLines`)
            if let Some(label) = &label_item {
                let label_value = if is_measure_value(label) {
                    Some(y_value.clone())
                } else {
                    find_index_in_order_local(response, label).map(|index| {
                        row.get("data")
                            .and_then(Value::as_array)
                            .and_then(|values| values.get(index))
                            .cloned()
                            .unwrap_or(Value::Null)
                    })
                };
                if let Some(label_value) = label_value {
                    if !label_value.is_null() {
                        let label_data_type = data_types
                            .get(s(label, "guid"))
                            .cloned()
                            .unwrap_or_else(|| data_type(label).to_string());
                        let stored = if is_numerical_data_type(&label_data_type) {
                            match number_value(&label_value) {
                                Some(number) => json!(number),
                                None => label_value,
                            }
                        } else if is_date_type(&label_data_type) {
                            json!(date_axis_value(&label_value, &label_data_type).unwrap_or(0))
                        } else {
                            label_value
                        };
                        lines[index].labels.insert(category_key, stored);
                    }
                }
            }
        }
    }

    // category sorting (`getSortedCategories`)
    let sort_item = sort.first().cloned();
    let is_sort_item_exists = !sort.is_empty();
    let is_sort_categories_available = if is_x_discrete { true } else { is_x_discrete };
    let is_sorting_x_axis = sort
        .iter()
        .any(|item| x.as_ref().map(|x| s(item, "guid") == s(x, "guid")).unwrap_or(false));
    let is_sorting_y_axis = sort_item
        .as_ref()
        .map(|item| merged_y.iter().any(|(field, _, _)| s(item, "guid") == s(field, "guid")))
        .unwrap_or(false);
    let is_sort_by_segments = sort_item
        .as_ref()
        .zip(segment_field.as_ref())
        .map(|(item, segment)| s(item, "guid") == s(segment, "guid"))
        .unwrap_or(false);
    let is_sortable_x_axis = visualization_id != "area" && !is_percent_visualization(&visualization_id);
    let is_sort_with_y_section_item = !y_items.is_empty() && is_sortable_x_axis;
    let is_sort_available = is_sort_item_exists && is_sort_categories_available;
    if x_is_date {
        categories.sort_by(|a, b| {
            let left = number_value(a).unwrap_or(0.0);
            let right = number_value(b).unwrap_or(0.0);
            left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    if !is_sort_available || is_sort_by_segments {
        if !x_is_date {
        categories.sort_by(|a, b| {
            if x_is_number || x_is_date {
                let left = number_value(a).unwrap_or(0.0);
                let right = number_value(b).unwrap_or(0.0);
                left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                collator_cmp(&a.to_string(), &b.to_string())
            }
        });
        }
    } else if is_sort_with_y_section_item {
        if let Some(sort_item) = &sort_item {
            let sort_title = original_or_title(sort_item);
            let sort_source = s(sort_item, "source");
            let matched = merged_y.iter().any(|(field, _, _)| {
                original_or_title(field) == sort_title
                    || (!sort_source.is_empty() && sort_source == s(field, "source"))
            });
            if matched {
                let field_title = fake_title(&merged_y[0].0);
                let mut cuts: HashMap<String, f64> = HashMap::new();
                let color_exists = color_item.as_ref().map(|field| s(field, "type") != "PSEUDO").unwrap_or(false);
                if color_exists || !segments_map.is_empty() {
                    for line in &lines {
                        for (category, point) in &line.data {
                            if let Some(value) = number_value(&point.value) {
                                if value == 0.0 {
                                    continue;
                                }
                                *cuts.entry(category.clone()).or_insert(0.0) += value;
                            }
                        }
                    }
                } else {
                    for line in &lines {
                        if line.field_title != field_title {
                            continue;
                        }
                        for (category, point) in &line.data {
                            if let Some(value) = number_value(&point.value) {
                                if value == 0.0 {
                                    continue;
                                }
                                *cuts.entry(category.clone()).or_insert(0.0) += value;
                            }
                        }
                    }
                }
                let direction = s(sort_item, "direction");
                let is_asc = direction == "ASC";
                categories.sort_by(|a, b| {
                    let left = cuts.get(&a.to_string()).copied().unwrap_or(0.0);
                    let right = cuts.get(&b.to_string()).copied().unwrap_or(0.0);
                    let greater = left > right;
                    if is_asc {
                        if greater { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less }
                    } else if greater {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                });
            }
        }
    }
    let _ = is_sorting_x_axis;
    let _ = is_sorting_y_axis;

    // line keys sorting (`getSortedLineKeys`) — applied per y section (y and y2 separately)
    let should_sort_collator = if !is_sort_available || is_sort_by_segments {
        color_item.is_none() || color_item.as_ref().map(|field| s(field, "type") != "PSEUDO").unwrap_or(true)
    } else {
        false
    };
    let mut line_order: Vec<usize> = Vec::new();
    for section in [false, true] {
        let mut indices: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.is_y2 == section)
            .map(|(index, _)| index)
            .collect();
        if should_sort_collator {
            indices.sort_by(|a, b| collator_cmp(&lines[*a].key, &lines[*b].key));
        }
        line_order.extend(indices);
    }
    if is_sort_available && color_item.is_some() {
        if let Some(sort_item) = &sort_item {
            let is_area = matches!(visualization_id.as_str(), "area" | "area100p");
            let sort_matches_color = color_item.as_ref().map(|field| s(field, "guid") == s(sort_item, "guid")).unwrap_or(false);
            if sort_matches_color && !is_measure_field(sort_item) && !is_area {
                for section in [false, true] {
                    let mut indices: Vec<usize> = line_order
                        .iter()
                        .copied()
                        .filter(|index| lines[*index].is_y2 == section)
                        .collect();
                    indices.sort_by(|a, b| collator_cmp(&lines[*a].key, &lines[*b].key));
                    let mut position = 0;
                    for index in line_order.iter_mut() {
                        if lines[*index].is_y2 == section {
                            *index = indices[position];
                            position += 1;
                        }
                    }
                }
            } else if is_area && s(sort_item, "guid") == placeholder_items(placeholders, "y").first().map(|item| s(item, "guid")).unwrap_or("") {
                // `sortLineKeysByFirstValues` (only relevant for charts with several series).
                let direction = s(sort_item, "direction").to_string();
                for section in [false, true] {
                    let mut indices: Vec<usize> = line_order
                        .iter()
                        .copied()
                        .filter(|index| lines[*index].is_y2 == section)
                        .collect();
                    let first_category = categories.first().map(|category| category.to_string()).unwrap_or_default();
                    indices.sort_by(|a, b| {
                        let left = lines[*a].data.get(&first_category).and_then(|point| number_value(&point.value));
                        let right = lines[*b].data.get(&first_category).and_then(|point| number_value(&point.value));
                        match (left, right) {
                            (Some(left), Some(right)) => {
                                let difference = if direction == "ASC" { left - right } else { right - left };
                                difference.partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal)
                            }
                            _ => std::cmp::Ordering::Equal,
                        }
                    });
                    let mut position = 0;
                    for index in line_order.iter_mut() {
                        if lines[*index].is_y2 == section {
                            *index = indices[position];
                            position += 1;
                        }
                    }
                }
            }
        }
    }

    let is_x_category_axis = is_x_discrete
        || x_data_type == "string"
        || x_is_pseudo
        || (is_sort_categories_available && is_sort_item_exists && x_is_number && !is_sorting_x_axis && (is_sorting_y_axis || false));
    let mut graphs: Vec<Value> = Vec::new();
    let mut unique_titles: Vec<String> = Vec::new();
    for line_index in line_order {
        let line = &lines[line_index];
        let nulls_value = line
            .nulls_setting
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null);
        let nulls = if is_bar_x {
            line.nulls_setting.clone().unwrap_or_default()
        } else {
            get_axis_nulls_settings(&nulls_value, &visualization_id)
        };
        let mut points: Vec<Value> = Vec::new();
        for (index, category) in categories.iter().enumerate() {
            let key = match category {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let raw = line.data.get(&key);
            let mut value = raw.map(|point| point.value.clone()).unwrap_or(Value::Null);
            if raw.is_none() && nulls == "as-0" {
                value = json!(0);
            }
            if !is_x_category_axis && raw.is_none() && (is_bar_x || nulls == "connect") {
                continue;
            }
            let y = match &value {
                Value::Number(_) => value.clone(),
                _ => Value::Null,
            };
            let mut point = Map::new();
            let x_value = if is_x_category_axis { json!(index) } else { category.clone() };
            point.insert("x".to_string(), x_value);
            point.insert("y".to_string(), y);
            if let Some(point_data) = raw {
                if let Some(color_value) = &point_data.color_value {
                    point.insert("colorValue".to_string(), color_value.clone());
                }
            }
            point.insert(
                "dataLabels".to_string(),
                json!({"enabled": !labels.is_empty() && label_item.is_some()}),
            );
            if let Some(segment_key) = &line.segment_name_key {
                if let Some((segment_index, _)) = segments_map_by_name(segment_key) {
                    let _ = segment_index;
                    let title = segment_key.trim_end_matches("__y2");
                    point.insert(
                        "custom".to_string(),
                        json!({"tooltipPointName": format!("{title}: {}", line.title)}),
                    );
                }
            }
            if !is_x_category_axis {
                if let Some(x_field) = &x {
                    if is_numerical_data_type(data_type(x_field)) {
                        let formatting = get_format_options(x_field);
                        let emit = if is_bar_x {
                            formatting.as_object().map(|map| !map.is_empty()).unwrap_or(false)
                        } else {
                            formatting.is_object()
                        };
                        if emit {
                            let number = number_value(category).unwrap_or(0.0);
                            point.insert(
                                "xFormatted".to_string(),
                                json!(format_number_chartkit(number, &formatting)),
                            );
                        }
                    }
                }
            }
            let label_value = line
                .labels
                .get(&key)
                .cloned()
                .unwrap_or_else(|| json!(""));
            point.insert("label".to_string(), label_value);
            points.push(Value::Object(point));
        }
        let mut graph = Map::new();
        let graph_id = line
            .legend_title
            .clone()
            .filter(|value| !value.is_empty())
            .or_else(|| if line.title.is_empty() { None } else { Some(line.title.clone()) })
            .unwrap_or_default();
        graph.insert("id".to_string(), json!(graph_id));
        graph.insert("title".to_string(), json!(line.title));
        let line_y_field = merged_y
            .iter()
            .find(|(field, _, _)| fake_title(field) == line.field_title)
            .map(|(field, _, _)| field.clone())
            .unwrap_or_else(|| merged_y.first().map(|(field, _, _)| field.clone()).unwrap_or(Value::Null));
        let line_y_data_type = data_types
            .get(s(&line_y_field, "guid"))
            .cloned()
            .unwrap_or_else(|| data_type(&line_y_field).to_string());
        let tooltip = chartkit_formatting_of(&get_format_options(&line_y_field), &line_y_data_type);
        graph.insert("tooltip".to_string(), tooltip);
        // dataLabels from `mergeLabelDataWithLines`
        let mut graph_data_labels = Map::new();
        if let Some(label) = &label_item {
            let hide_label = s(
                merged_y
                    .iter()
                    .find(|(field, _, _)| fake_title(field) == line.field_title)
                    .map(|(field, _, _)| field)
                    .unwrap_or(&Value::Null),
                "hideLabelMode",
            ) == "hide";
            if hide_label {
                graph_data_labels.insert("enabled".to_string(), json!(false));
            } else {
                let is_label_pseudo = is_pseudo_field(label);
                let label_data_type = if is_label_pseudo {
                    data_types
                        .get(s(
                            merged_y
                                .iter()
                                .find(|(field, _, _)| fake_title(field) == line.field_title)
                                .map(|(field, _, _)| field)
                                .unwrap_or(&Value::Null),
                            "guid",
                        ))
                        .cloned()
                        .unwrap_or_else(|| data_type(&merged_y[0].0).to_string())
                } else {
                    data_types
                        .get(s(label, "guid"))
                        .cloned()
                        .unwrap_or_else(|| data_type(label).to_string())
                };
                let label_formatting = if is_label_pseudo {
                    merged_y
                        .iter()
                        .find(|(field, _, _)| fake_title(field) == line.field_title)
                        .map(|(field, _, _)| get_format_options(field))
                        .unwrap_or(Value::Null)
                } else {
                    get_format_options(label)
                };
                let formatted = chartkit_formatting_of(&label_formatting, &label_data_type);
                if let Value::Object(map) = formatted {
                    for (key, value) in map {
                        graph_data_labels.insert(key, value);
                    }
                }
            }
        } else {
            graph_data_labels.insert("enabled".to_string(), json!(false));
        }
        graph_data_labels.insert("useHTML".to_string(), json!(false));
        graph.insert("dataLabels".to_string(), Value::Object(graph_data_labels));
        graph.insert("data".to_string(), Value::Array(points));
        graph.insert(
            "legendTitle".to_string(),
            json!(line
                .legend_title
                .clone()
                .or_else(|| Some(line.title.clone()))
                .unwrap_or_else(|| "Null".to_string())),
        );
        if color_item.is_none() {
            if is_bar_x {
                if let Some(formatted_name) = &line.formatted_name {
                    graph.insert("formattedName".to_string(), json!(formatted_name));
                }
            } else {
                graph.insert(
                    "formattedName".to_string(),
                    json!(series_name_formatter(&line.formatted_name.clone().map(Value::String).unwrap_or(Value::Null))),
                );
            }
        }
        if let Some(drill_down) = &line.drill_down_filter_value {
            graph.insert("drillDownFilterValue".to_string(), json!(drill_down));
        }
        if let Some(color_key) = &line.color_key {
            graph.insert("colorKey".to_string(), json!(color_key));
        }
        graph.insert(
            "colorGuid".to_string(),
            json!(color_item.as_ref().map(|field| s(field, "guid")).filter(|guid| !guid.is_empty())),
        );
        if !is_bar_x {
            graph.insert(
                "shapeGuid".to_string(),
                json!(shape_item.as_ref().map(|field| s(field, "guid")).filter(|guid| !guid.is_empty())),
            );
        }
        graph.insert("measureFieldTitle".to_string(), json!(line.field_title));
        if !is_bar_x {
            if points_len(&graph) > 1 {
                graph.insert("connectNulls".to_string(), json!(nulls == "connect"));
            }
        } else {
            graph.insert("connectNulls".to_string(), json!(nulls == "connect"));
        }
        if line.point_conflict {
            graph.insert("pointConflict".to_string(), json!(true));
        }
        let y_axis_index = match segments_map_by_name(line.segment_name_key.as_deref().unwrap_or("")) {
            Some((segment_index, _)) => segment_index,
            None => {
                if line.is_y2 {
                    1
                } else {
                    0
                }
            }
        };
        graph.insert("yAxis".to_string(), json!(y_axis_index));
        if let Some(stack) = &line.stack {
            graph.insert("stack".to_string(), stack.clone());
        }
        let mut custom = Map::new();
        if let Some(segment_key) = &line.segment_name_key {
            let title = segment_key.trim_end_matches("__y2").to_string();
            custom.insert("segmentTitle".to_string(), json!(title));
        }
        graph.insert("custom".to_string(), Value::Object(custom));
        if let Some(color_value) = &line.color_value {
            graph.insert("colorValue".to_string(), color_value.clone());
        }
        if let Some(shape_value) = &line.shape_value {
            graph.insert("shapeValue".to_string(), json!(shape_value));
        }
        if unique_titles.iter().any(|title| {
            graph
                .get("legendTitle")
                .and_then(Value::as_str)
                .map(|legend| legend == title)
                .unwrap_or(false)
        }) {
            graph.insert("showInLegend".to_string(), json!(false));
        } else if let Some(legend_title) = graph.get("legendTitle").and_then(Value::as_str) {
            unique_titles.push(legend_title.to_string());
        }
        graphs.push(Value::Object(graph));
    }
    colorize_layer_graphs(
        &mut graphs,
        &colors_config,
        is_color_item_exist,
        is_shape_item_exist,
        !segments_map.is_empty(),
        color_item.as_ref(),
        context.used_colors,
        context.default_palette_id,
    );
    if visualization_id == "line" {
        let shapes_config = if visualizations_with_layers(&visualization) {
            common.get("shapesConfig").cloned().unwrap_or_else(|| json!({}))
        } else {
            config.get("shapesConfig").cloned().unwrap_or_else(|| json!({}))
        };
        map_and_shape_graph(
            &mut graphs,
            shapes_config,
            !segments_map.is_empty(),
            shapes.is_empty() || shapes.first().map(is_pseudo_field).unwrap_or(false),
        );
    }
    let mut result = Map::new();
    result.insert("graphs".to_string(), Value::Array(graphs));
    if is_x_category_axis {
        let formatter_field = json!({
            "data_type": if x_data_type.is_empty() { s(x.as_ref().unwrap_or(&Value::Null), "data_type") } else { &x_data_type },
            "format": x.as_ref().and_then(|field| field.get("format")).cloned().unwrap_or(Value::Null),
        });
        let formatted: Vec<Value> = categories
            .iter()
            .map(|category| json!(format_category(&formatter_field, category)))
            .collect();
        result.insert("categories".to_string(), Value::Array(formatted));
    }
    let _ = extra;
    Ok(Value::Object(result))
}

fn points_len(graph: &Map<String, Value>) -> usize {
    graph
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn find_index_in_order_local(response: &Value, field: &Value) -> Option<usize> {
    // Response fields carry `legend_item_id`; the first row legend defines the column order.
    let legend: Vec<i64> = response
        .pointer("/result_data/0/rows/0/legend")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let fields = response.get("fields").and_then(Value::as_array)?;
    let guid = s(field, "guid");
    let title = s(field, "title");
    let target = fields.iter().find(|item| s(item, "id") == guid).or_else(|| fields.iter().find(|item| s(item, "title") == title))?;
    let legend_id = target.get("legend_item_id").and_then(Value::as_i64)?;
    legend.iter().position(|item| *item == legend_id)
}

fn get_colored_line_legend_title(
    config: &Value,
    y_item: &Value,
    color_item: &Value,
    formatted_value: &str,
    visualization: &Value,
) -> String {
    let layers = array(visualization, "layers");
    let colored_layers: Vec<&Value> = layers
        .iter()
        .filter(|layer| {
            let y_items = array(layer, "placeholders")
                .iter()
                .find(|placeholder| s(placeholder, "id") == "y")
                .map(|placeholder| !array(placeholder, "items").is_empty())
                .unwrap_or(false);
            let colors = layer
                .pointer("/commonPlaceholders/colors")
                .and_then(Value::as_array)
                .map(|items| !items.is_empty())
                .unwrap_or(false);
            y_items && colors
        })
        .collect();
    let _ = config;
    if colored_layers.len() > 1 {
        let is_different_y_item = colored_layers.iter().any(|layer| {
            placeholder_items(array(layer, "placeholders"), "y")
                .iter()
                .any(|item| fake_title(item) != fake_title(y_item))
        });
        let is_different_color_item = colored_layers.iter().any(|layer| {
            array(layer.pointer("/commonPlaceholders").unwrap_or(&Value::Null), "colors")
                .iter()
                .any(|item| is_dimension_field(item) && fake_title(item) != fake_title(color_item))
        });
        let mut parts: Vec<String> = vec![formatted_value.to_string()];
        if is_different_color_item {
            parts.insert(0, fake_title(color_item));
        }
        if is_different_y_item {
            parts.insert(0, fake_title(y_item));
        }
        return parts.join(": ");
    }
    formatted_value.to_string()
}

fn map_and_shape_graph(
    graphs: &mut [Value],
    shapes_config: Value,
    is_segments_exists: bool,
    is_shapes_default: bool,
) {
    let shapes_order = [
        "Solid",
        "Dash",
        "Dot",
        "ShortDashDot",
        "LongDash",
        "LongDashDot",
        "ShortDot",
        "LongDashDotDot",
        "ShortDash",
        "DashDot",
        "ShortDashDotDot",
    ];
    let mounted = shapes_config
        .get("mountedShapes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut known_values: Vec<String> = Vec::new();
    for (index, graph) in graphs.iter_mut().enumerate() {
        let shape_value = graph
            .get("shapeValue")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = if !shape_value.is_empty() {
            shape_value.clone()
        } else {
            graph
                .get("legendTitle")
                .and_then(Value::as_str)
                .or_else(|| graph.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_string()
        };
        if let Some(mounted_shape) = mounted.get(&title) {
            if mounted_shape.as_str() != Some("auto") {
                graph["dashStyle"] = mounted_shape.clone();
                continue;
            }
        }
        if is_shapes_default {
            graph["dashStyle"] = json!(shapes_order[0]);
        } else {
            let y_axis_zero = graph.get("yAxis").and_then(Value::as_i64).unwrap_or(0) == 0;
            let mut shape_index = if y_axis_zero || shape_value.is_empty() || is_segments_exists {
                known_values.iter().position(|value| value == &shape_value)
            } else {
                Some(index)
            };
            if shape_index.is_none() {
                known_values.push(shape_value);
                shape_index = Some(known_values.len() - 1);
            }
            let shape_index = shape_index.unwrap_or(index);
            graph["dashStyle"] = json!(shapes_order[shape_index % shapes_order.len()]);
        }
    }
}

fn colorize_layer_graphs(
    graphs: &mut [Value],
    colors_config: &Value,
    is_colors_item_exists: bool,
    is_shapes_item_exists: bool,
    is_segments_exists: bool,
    color_field: Option<&Value>,
    used_colors: &mut Vec<String>,
    default_palette_id: &str,
) {
    let (mounted_colors, colors) = get_colors_settings(colors_config, color_field, default_palette_id);
    for (index, graph) in graphs.iter_mut().enumerate() {
        let color_value = graph.get("colorValue").and_then(Value::as_str).unwrap_or("").to_string();
        let shape_value = graph.get("shapeValue").and_then(Value::as_str).unwrap_or("").to_string();
        let color_title = graph
            .get("colorKey")
            .and_then(Value::as_str)
            .or_else(|| graph.get("legendTitle").and_then(Value::as_str))
            .or_else(|| graph.get("name").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let color_key = if (!color_value.is_empty() && color_value == color_title)
            || (!color_value.is_empty() && !shape_value.is_empty())
        {
            Some(color_value.clone())
        } else if is_colors_item_exists {
            Some(color_title.split(':').next().unwrap_or("").to_string())
        } else {
            Some(color_title.clone())
        };
        if let Some(color_key) = color_key.as_deref() {
            if !color_key.is_empty() {
                if let Some(color) = mounted_color(&mounted_colors, &colors, color_key) {
                    graph["color"] = json!(color);
                    continue;
                }
            }
        }
        let mut value = graph.get("colorValue").and_then(Value::as_str).unwrap_or("").to_string();
        if is_colors_item_exists && !is_shapes_item_exists {
            let legend = graph.get("legendTitle").and_then(Value::as_str).unwrap_or("");
            if !legend.is_empty() {
                value = legend.to_string();
            }
        }
        let color_index = if is_shapes_item_exists && !is_colors_item_exists {
            used_colors.iter().position(|used| used == &value)
        } else {
            let y_axis_zero = graph.get("yAxis").and_then(Value::as_i64).unwrap_or(0) == 0;
            if y_axis_zero || is_segments_exists {
                used_colors.iter().position(|used| used == &value)
            } else {
                Some(index)
            }
        };
        let color_index = match color_index {
            Some(index) => index,
            None => {
                used_colors.push(value);
                used_colors.len() - 1
            }
        };
        graph["color"] = json!(get_color(color_index, &colors));
    }
}

fn format_category(field: &Value, value: &Value) -> String {
    let value_type = s(field, "data_type");
    if is_date_type(value_type) || parse_date_ms(value).is_some() {
        let millis = number_value(value)
            .map(|millis| millis as i64)
            .or_else(|| parse_date_ms(value))
            .unwrap_or(0);
        let format = field
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or(default_date_format(if value_type.is_empty() { "genericdatetime" } else { value_type }));
        return format_date_ms(millis, format);
    }
    match value {
        Value::String(text) => {
            if text.is_empty() {
                "Null".to_string()
            } else {
                text.clone()
            }
        }
        Value::Null => "Null".to_string(),
        Value::Number(number) => number.as_f64().map(number_to_js_string).unwrap_or_else(|| number.to_string()),
        other => other.to_string(),
    }
}

fn extend_combined_graphs(graphs: &mut [Value], layer: &Value, layers: &[Value]) {
    let char_type = s(layer, "id");
    let layer_settings_id = s(layer.pointer("/layerSettings").unwrap_or(&Value::Null), "id");
    for graph in graphs.iter_mut() {
        graph["type"] = json!(char_type);
        let base_id = graph
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| graph.get("title").and_then(Value::as_str))
            .unwrap_or("");
        graph["id"] = json!(format!("{base_id}__{layer_settings_id}"));
        if !graph.get("colorGuid").map(Value::is_null).unwrap_or(true)
            && layers.len() > 1 {
                let measure = graph.get("measureFieldTitle").and_then(Value::as_str).unwrap_or("");
                let title = graph.get("title").and_then(Value::as_str).unwrap_or("");
                graph["title"] = json!(format!("{measure}: {title}"));
            }
        match char_type {
            "line" => {
                let placeholders = array(layer, "placeholders");
                let has_y = !placeholder_items(placeholders, "y").is_empty();
                let has_y2 = !placeholder_items(placeholders, "y2").is_empty();
                if !has_y && has_y2 && layers.len() > 1 {
                    graph["yAxis"] = json!(1);
                }
            }
            "column" => {
                let stack = graph
                    .get("stack")
                    .map(|value| match value {
                        Value::String(text) => text.clone(),
                        Value::Number(number) => number.as_f64().map(number_to_js_string).unwrap_or_else(|| number.to_string()),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                graph["stack"] = json!(format!("{layer_settings_id}__{stack}"));
            }
            "area" => {
                graph["stack"] = json!(layer_settings_id);
            }
            _ => {}
        }
    }
}

fn is_chart_without_data(graph: &Value) -> bool {
    let data = graph.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    if data.is_empty() {
        return true;
    }
    data.iter().all(Value::is_null)
}

fn merge_combined_result(first: &mut Value, rest: Vec<Value>) {
    let mut graphs: Vec<Value> = first
        .get("graphs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for item in rest {
        if let Some(other) = item.get("graphs").and_then(Value::as_array) {
            graphs.extend(other.iter().cloned());
        }
    }
    let has_one_graph_with_data = graphs.iter().any(|graph| !is_chart_without_data(graph));
    if has_one_graph_with_data && graphs.first().map(is_chart_without_data).unwrap_or(false) {
        if let Some(map) = first.as_object_mut() {
            map.remove("categories_ms");
        }
    }
    if has_one_graph_with_data {
        graphs.retain(|graph| !is_chart_without_data(graph));
    }
    if let Some(map) = first.as_object_mut() {
        map.insert("graphs".to_string(), Value::Array(graphs));
    }
}

// ---------------------------------------------------------------------------------------------
// `mergeData` (`js/js.ts:104-370`): join the per-dataset answers through `links`
// ---------------------------------------------------------------------------------------------

fn field_by_guid<'a>(fields: &'a [Value], guid: &str) -> Option<&'a Value> {
    fields.iter().find(|field| s(field, "guid") == guid)
}

fn field_by_legend(fields: &[Value], legend_item_id: i64) -> Option<&Value> {
    fields
        .iter()
        .find(|field| field.get("legend_item_id").and_then(Value::as_i64) == Some(legend_item_id))
}

fn role_of(field: &Value) -> &str {
    field
        .pointer("/role_spec/role")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// JS `String(value)` for an array cell as `Array.prototype.join()` renders it.
fn js_cell_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => {
            number.as_f64().map(number_to_js_string).unwrap_or_else(|| number.to_string())
        }
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| js_cell_string(Some(item)))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// `markupToRawString` (`shared/modules/wizard-helpers.ts:110`).
fn markup_to_raw_string(value: &Value) -> String {
    let mut text = String::new();
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            match child {
                Value::String(part) => text.push_str(part),
                other => text.push_str(&markup_to_raw_string(other)),
            }
        }
    } else if let Some(content) = value.get("content") {
        match content {
            Value::String(part) => text.push_str(part),
            other => text.push_str(&markup_to_raw_string(other)),
        }
    }
    text
}

/// `moment.utc(value).valueOf()` for the date formats data-api emits.
fn utc_millis(value: &str) -> String {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return parsed.timestamp_millis().to_string();
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return parsed.and_utc().timestamp_millis().to_string();
        }
        if let Ok(parsed) = chrono::NaiveDate::parse_from_str(value, format) {
            return parsed
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc()
                .timestamp_millis()
                .to_string();
        }
    }
    "NaN".to_string()
}

/// `getValueForCompare` (`js/js.ts:95`): the join key part for one linked column.
fn value_for_compare(value: Option<&Value>, field: &Value, other: &Value) -> String {
    let data_type = s(field, "dataType");
    let other_data_type = s(other, "dataType");
    if data_type == "datetimetz" || (is_date_type(data_type) && data_type != other_data_type) {
        return match value {
            Some(Value::String(text)) => utc_millis(text),
            Some(Value::Number(number)) => number.to_string(),
            _ => "NaN".to_string(),
        };
    }
    if data_type == "markup" {
        if let Some(item) = value {
            if item.get("type").and_then(Value::as_str).is_some() {
                return markup_to_raw_string(item);
            }
        }
    }
    js_cell_string(value)
}

fn order_item_title(item: &Value) -> String {
    s(item, "title").to_string()
}

fn first_order_item(item: &Value) -> Value {
    item.as_array()
        .and_then(|items| items.first())
        .cloned()
        .unwrap_or_else(|| item.clone())
}

fn mapped_response_fields(body: &Value) -> Vec<Value> {
    body.get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .map(|field| {
                    let mut field = field.clone();
                    let id = s(&field, "id").to_string();
                    field["guid"] = json!(id);
                    field
                })
                .collect()
        })
        .unwrap_or_default()
}

fn current_order_of(result_fields: &[Value], legend: &[Value]) -> Vec<Value> {
    legend
        .iter()
        .filter_map(Value::as_i64)
        .filter_map(|legend_id| field_by_legend(result_fields, legend_id))
        .map(|field| {
            json!({
                "guid": s(field, "id"),
                "title": s(field, "title"),
                "dataType": s(field, "data_type"),
            })
        })
        .collect()
}

/// `mergeData` (`js/js.ts:113-368`) for the non-layer path and for one visualization layer.
/// Returns a single API-v2-shaped response (`fields` + `result_data[0].rows`) that the existing
/// single-dataset builder consumes unchanged.
fn merge_responses(config: &Value, fetched: &[&GraphFetched]) -> Result<Value, String> {
    let links = array(config, "links");
    let mut fields: Vec<Value> = Vec::new();
    let mut field_datasets: Vec<String> = Vec::new();
    let mut rows: Vec<Value> = Vec::new();
    let mut order: Vec<Value> = Vec::new();
    let mut master_dataset: Option<String> = None;

    for response in fetched {
        let current = response.dataset.clone();
        let mapped_fields = mapped_response_fields(&response.body);
        for field in &mapped_fields {
            fields.push(field.clone());
            field_datasets.push(current.clone());
        }
        let result_fields: Vec<Value> = response
            .body
            .get("fields")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut dataset_rows: Vec<Value> = response
            .body
            .pointer("/result_data/0/rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if dataset_rows.is_empty() {
            continue;
        }
        let last_row = dataset_rows.last().cloned().unwrap_or(Value::Null);
        let last_legend: Vec<i64> = last_row
            .get("legend")
            .and_then(Value::as_array)
            .map(|legend| legend.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let result_contains_totals = last_legend.iter().any(|legend_id| {
            field_by_legend(&result_fields, *legend_id).map(role_of) == Some("total")
        });
        // A template-only placeholder row (a dimension of a dataset that only carries totals)
        // breaks the chart; `mergeData` removes it.
        let is_empty_placeholder = last_legend.len() == 1
            && field_by_legend(&result_fields, last_legend[0]).map(role_of) == Some("template");
        if result_contains_totals || is_empty_placeholder {
            dataset_rows.pop();
        }
        if dataset_rows.is_empty() {
            continue;
        }
        let is_first = rows.is_empty();
        if is_first {
            master_dataset = Some(current.clone());
            for row in &dataset_rows {
                rows.push(json!({
                    "data": row.get("data").cloned().unwrap_or_else(|| json!([])),
                    "legend": row.get("legend").cloned().unwrap_or_else(|| json!([])),
                }));
            }
            let first_legend: Vec<i64> = dataset_rows[0]
                .get("legend")
                .and_then(Value::as_array)
                .map(|legend| legend.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            for legend_id in first_legend {
                if let Some(field) = field_by_legend(&result_fields, legend_id) {
                    order.push(json!({
                        "datasetId": current,
                        "title": s(field, "title"),
                        "dataType": s(field, "data_type"),
                    }));
                }
            }
            continue;
        }
        let master = master_dataset.clone().unwrap_or_default();
        let current_order = current_order_of(
            &result_fields,
            dataset_rows[0]
                .get("legend")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        let fields_in_master: Vec<(String, String)> = order
            .iter()
            .map(|item| {
                let item = first_order_item(item);
                (s(&item, "title").to_string(), s(&item, "dataType").to_string())
            })
            .collect();
        let needed_links: Vec<&Value> = links
            .iter()
            .filter(|link| {
                link_field_guid(link, &current).is_some() && link_field_guid(link, &master).is_some()
            })
            .collect();
        // `indexInMasterByIndexInCurrent` / `indexInCurrentByLinkIndex`.
        let mut index_in_master: HashMap<usize, usize> = HashMap::new();
        for link in &needed_links {
            let current_guid = link_field_guid(link, &current).unwrap_or("");
            let master_guid = link_field_guid(link, &master).unwrap_or("");
            let current_field = field_by_guid(&fields, current_guid)
                .cloned()
                .unwrap_or_else(|| json!({"title": ""}));
            let master_field = field_by_guid(&fields, master_guid)
                .cloned()
                .unwrap_or_else(|| json!({"title": ""}));
            let index_in_master_of_link = fields_in_master
                .iter()
                .position(|(title, _)| title == s(&master_field, "title"));
            let index_in_current = current_order
                .iter()
                .position(|item| s(item, "title") == s(&current_field, "title"));
            if let (Some(index_in_master_of_link), Some(index_in_current)) =
                (index_in_master_of_link, index_in_current)
            {
                index_in_master.insert(index_in_current, index_in_master_of_link);
            }
        }
        // `currentOrder.forEach`: keep unlinked fields in order, nest linked ones at the master
        // position (a linked field exists in both datasets but has one column of the merged row).
        for (index, item) in current_order.iter().enumerate() {
            let result_order_item = json!({
                "datasetId": current,
                "title": s(item, "title"),
                "dataType": s(item, "dataType"),
            });
            match index_in_master.get(&index).copied() {
                None => order.push(result_order_item),
                Some(master_index) => {
                    if !order[master_index].is_array() {
                        let existing = order[master_index].clone();
                        order[master_index] = json!([existing]);
                    }
                    if let Some(items) = order[master_index].as_array_mut() {
                        items.push(result_order_item);
                    }
                }
            }
        }
        let link_order: Vec<(Option<usize>, usize)> = needed_links
            .iter()
            .map(|link| {
                let master_title = link_field_guid(link, &master)
                    .and_then(|guid| field_by_guid(&fields, guid))
                    .map(|field| s(field, "title").to_string())
                    .unwrap_or_default();
                let current_title = link_field_guid(link, &current)
                    .and_then(|guid| field_by_guid(&fields, guid))
                    .map(|field| s(field, "title").to_string())
                    .unwrap_or_default();
                let left = order.iter().position(|item| {
                    order_item_title(&first_order_item(item)) == master_title
                });
                let right = current_order
                    .iter()
                    .position(|item| order_item_title(item) == current_title);
                (left, right.unwrap_or(usize::MAX))
            })
            .collect();
        let usable_link_order: Vec<(Option<usize>, usize)> = link_order
            .iter()
            .filter(|(_, right)| *right != usize::MAX)
            .copied()
            .collect();
        // `sourceDataMap`: rows of the already merged datasets indexed by their join key.
        let mut source_data_map: HashMap<String, usize> = HashMap::new();
        for (row_index, row) in rows.iter().enumerate() {
            let data = row.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let join_by = usable_link_order
                .iter()
                .map(|(left, right)| {
                    let left_item = left
                        .and_then(|left| order.get(left))
                        .map(first_order_item)
                        .unwrap_or_else(|| json!({}));
                    let right_item = current_order.get(*right).cloned().unwrap_or_else(|| json!({}));
                    value_for_compare(left.and_then(|left| data.get(left)), &left_item, &right_item)
                })
                .collect::<Vec<_>>()
                .join(",");
            source_data_map.insert(join_by, row_index);
        }
        for row in &dataset_rows {
            let data = row.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            let join_by = usable_link_order
                .iter()
                .map(|(left, right)| {
                    let current_item = current_order.get(*right).cloned().unwrap_or_else(|| json!({}));
                    let other_item = left
                        .and_then(|left| order.get(left))
                        .map(first_order_item)
                        .unwrap_or_else(|| json!({}));
                    value_for_compare(data.get(*right), &current_item, &other_item)
                })
                .collect::<Vec<_>>()
                .join(",");
            let mut possible: Vec<Value> = Vec::new();
            for (left, right) in &usable_link_order {
                if let Some(left) = left {
                    if possible.len() <= *left {
                        possible.resize(*left + 1, Value::Null);
                    }
                    possible[*left] = data.get(*right).cloned().unwrap_or(Value::Null);
                }
            }
            let unlinked: Vec<Value> = data
                .iter()
                .enumerate()
                .filter(|(index, _)| !index_in_master.contains_key(index))
                .map(|(_, value)| value.clone())
                .collect();
            let target = source_data_map.get(&join_by).copied().filter(|index| {
                rows[*index]
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|target| target.len() + unlinked.len() <= order.len())
                    .unwrap_or(false)
            });
            let target_index = match target {
                Some(index) => index,
                None => {
                    rows.push(json!({
                        "data": possible,
                        "legend": row.get("legend").cloned().unwrap_or_else(|| json!([])),
                    }));
                    rows.len() - 1
                }
            };
            if let Some(target) = rows[target_index].get_mut("data").and_then(Value::as_array_mut) {
                while order.len() > target.len() + unlinked.len() {
                    target.push(Value::Null);
                }
                target.extend(unlinked);
            }
        }
    }

    // Build the synthetic API response: every merged field keeps its metadata, and the columns
    // of `result.order` get a `legend_item_id` equal to their position so the existing
    // `response_index_by_guid` maps guids to the merged row positions.
    // A nested order entry is *one* merged column shared by both sides of a link
    // (`findIndexInOrder` returns the outer index for either field), so the position is the
    // outer index, not the index in the flattened list.
    let flat_order: Vec<(usize, Value)> = order
        .iter()
        .enumerate()
        .flat_map(|(position, item)| match item {
            Value::Array(items) => items
                .iter()
                .map(|item| (position, item.clone()))
                .collect::<Vec<_>>(),
            other => vec![(position, other.clone())],
        })
        .collect();
    let mut synthetic_fields: Vec<Value> = Vec::new();
    for (field, dataset) in fields.iter().zip(field_datasets.iter()) {
        let mut field = field.clone();
        let position = flat_order.iter().find_map(|(position, item)| {
            (s(item, "datasetId") == dataset.as_str() && s(item, "title") == s(&field, "title"))
                .then_some(*position)
        });
        match position {
            Some(position) => field["legend_item_id"] = json!(position as i64),
            None => {
                if let Some(object) = field.as_object_mut() {
                    object.remove("legend_item_id");
                }
            }
        }
        synthetic_fields.push(field);
    }
    let synthetic_rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "data": row.get("data").cloned().unwrap_or_else(|| json!([])),
                "legend": (0..order.len() as i64).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "fields": synthetic_fields,
        "result_data": [{"rows": synthetic_rows}],
    }))
}

fn layer_key_suffix(key: &str) -> &str {
    key.split_once("_result").map(|(_, suffix)| suffix).unwrap_or("")
}

pub fn build_data(config: &Value, fetched: &[GraphFetched], default_palette_id: &str) -> Result<Value, String> {
    let visualization = config.get("visualization").cloned().unwrap_or_else(|| json!({}));
    let layers = array(&visualization, "layers").to_vec();
    let mut used_colors: Vec<String> = Vec::new();
    if layers.is_empty() {
        if fetched.is_empty() {
            return Err("graph: no fetched source".to_string());
        }
        let sources: Vec<&GraphFetched> = fetched.iter().collect();
        // A single dataset is passed through untouched: `mergeData` is an identity there and
        // the exact wire shape (legend ids included) is what the goldens pin.
        let response = if sources.len() == 1 {
            sources[0].body.clone()
        } else {
            merge_responses(config, &sources)?
        };
        let placeholders = array(&visualization, "placeholders").to_vec();
        let common = json!({
            "colors": array(config, "colors"),
            "colorsConfig": config.get("colorsConfig").cloned().unwrap_or_else(|| json!({})),
            "sort": array(config, "sort"),
            "labels": array(config, "labels"),
            "tooltips": array(config, "tooltips"),
            "shapes": array(config, "shapes"),
            "segments": array(config, "segments"),
        });
        let layer = json!({"id": s(&visualization, "id")});
        let mut context = LayerContext {
            config,
            layer: &layer,
            placeholders: &placeholders,
            common: &common,
            used_colors: &mut used_colors,
            default_palette_id,
        };
        return build_layer_data(&mut context, &response);
    }
    let mut results: Vec<Value> = Vec::new();
    for layer in layers.iter() {
        let layer_id = layer
            .pointer("/layerSettings/id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let sources: Vec<&GraphFetched> = fetched
            .iter()
            .filter(|response| layer_key_suffix(&response.key) == layer_id)
            .collect();
        if sources.is_empty() {
            return Err(format!("graph: no fetched source for layer {layer_id}"));
        }
        let response = if sources.len() == 1 {
            sources[0].body.clone()
        } else {
            merge_responses(config, &sources)?
        };
        let placeholders = array(layer, "placeholders").to_vec();
        let common = layer.get("commonPlaceholders").cloned().unwrap_or_else(|| json!({}));
        let mut context = LayerContext {
            config,
            layer,
            placeholders: &placeholders,
            common: &common,
            used_colors: &mut used_colors,
            default_palette_id,
        };
        let mut layer_data = build_layer_data(&mut context, &response)?;
        if let Some(graphs) = layer_data.get_mut("graphs").and_then(Value::as_array_mut) {
            extend_combined_graphs(graphs, layer, &layers);
        }
        results.push(layer_data);
    }
    let mut first = results.remove(0);
    merge_combined_result(&mut first, results);
    Ok(first)
}

/// Finished config: base config + `userConfigOverride`, and the end-of-pipeline pieces.
pub fn finish(prepared: &GraphPrepared, fetched: &[GraphFetched], default_palette_id: &str) -> Result<GraphFinished, String> {
    // Borrow the prepared config: the old `prepared.config.clone()` deep-copied the whole
    // wizard config (all placeholders, axes, formatting) on every graph request.
    let config = &prepared.config;
    let mut output_config = build_config_from_server_config(config, &prepared.params);
    merge_json(&mut output_config, config_override(config));
    if !prepared.drill_down.is_null() {
        let mut drill_down = Map::new();
        drill_down.insert(
            "breadcrumbs".to_string(),
            prepared.drill_down.get("breadcrumbs").cloned().unwrap_or_else(|| json!([])),
        );
        let data_type = prepared
            .drill_down
            .get("fields")
            .and_then(Value::as_array)
            .and_then(|fields| fields.get(prepared.drill_down.get("level").and_then(Value::as_u64).unwrap_or(0) as usize))
            .map(|field| s(field, "data_type"))
            .unwrap_or("");
        let date_format = if data_type == "datetime" || data_type == "genericdatetime" {
            "YYYY-MM-DDTHH:mm:ss"
        } else {
            "YYYY-MM-DD"
        };
        drill_down.insert("dateFormat".to_string(), json!(date_format));
        output_config["drillDown"] = Value::Object(drill_down);
    }
    let data = build_data(config, fetched, default_palette_id)?;
    let graphs = data.get("graphs").cloned().unwrap_or_else(|| json!([]));
    let library = build_library(config, &graphs)?;
    let mut sources = Map::new();
    let mut data_export = Value::Null;
    for (request, response) in prepared.requests.iter().zip(fetched.iter()) {
        let mut source = Map::new();
        source.insert("sourceId".to_string(), json!(request.key));
        source.insert("sourceType".to_string(), json!("bi_datasets"));
        source.insert("status".to_string(), json!(response.status));
        source.insert("latency".to_string(), json!(response.latency));
        source.insert("size".to_string(), json!(response.size));
        source.insert("datasetId".to_string(), json!(request.dataset));
        source.insert("data".to_string(), request.body.clone());
        // `ChartEditor.setDataSourceInfo(key, {query})`: the SQL inspector reads the
        // concatenated `blocks[].query` of the data-api answer.
        let query = response
            .body
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
        if !query.is_empty() {
            source.insert("info".to_string(), json!({"query": query}));
        }
        sources.insert(request.key.clone(), Value::Object(source));
        if data_export.is_null() {
            if let Some(export) = &response.data_export {
                data_export = export.clone();
            }
        }
    }
    Ok(GraphFinished {
        data: GraphData::Value(data),
        config: output_config,
        library,
        sources: Value::Object(sources),
    })
}

/// Convenience entry point used by the offline oracle harness and by `run_rust_graph`.
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
mod graph_remainder_tests {
    use super::*;

    fn response(dataset: &str, fields: Value, rows: Value) -> GraphFetched {
        GraphFetched {
            key: format!("{dataset}_result"),
            dataset: dataset.to_string(),
            status: 200,
            latency: 0.0,
            size: 0,
            body: json!({"fields": fields, "result_data": [{"rows": rows}]}),
            data_export: None,
        }
    }

    fn refs(body: &Value) -> Vec<String> {
        array(body, "fields")
            .iter()
            .map(|field| s(field.pointer("/ref").unwrap_or(&Value::Null), "id").to_string())
            .collect()
    }

    fn config_with_link() -> Value {
        json!({
            "links": [{
                "fields": {
                    "A": {"field": {"guid": "a_date", "title": "date"}},
                    "B": {"field": {"guid": "b_date", "title": "bdate"}}
                }
            }]
        })
    }

    #[test]
    fn linked_sort_filter_and_join_columns_are_translated() {
        let config = config_with_link();
        let links = array(&config, "links");
        let placeholders = vec![
            json!({"id": "x", "items": [{"guid": "a_date", "title": "date", "datasetId": "A"}]}),
            json!({"id": "y", "items": [{"guid": "a_val", "title": "val", "datasetId": "A",
                    "type": "MEASURE", "data_type": "integer"}]}),
        ];
        let sort = vec![json!({"datasetId": "B", "guid": "b_date", "direction": "asc"})];
        let filters = vec![json!({
            "datasetId": "B",
            "guid": "b_date",
            "filter": {"operation": {"code": "IN"}, "value": ["2020-01-01"]}
        })];
        let body_a = build_request_body(
            &config, &json!({}), &placeholders, &filters, &[], &[], &[], &[], &[], &sort, links,
            "A", &[], &Value::Null, &json!({}),
        )
        .expect("request A");
        assert_eq!(refs(&body_a), vec!["a_date", "a_val"]);
        assert_eq!(
            body_a.pointer("/order_by/0/ref/id").and_then(Value::as_str),
            Some("a_date"),
            "linked sort must use the local side of the link: {body_a}"
        );
        assert_eq!(
            body_a.pointer("/filters/0/ref/id").and_then(Value::as_str),
            Some("a_date"),
            "linked filter must use the local side of the link: {body_a}"
        );

        let body_b = build_request_body(
            &config, &json!({}), &placeholders, &filters, &[], &[], &[], &[], &[], &[], links,
            "B", &[], &Value::Null, &json!({}),
        )
        .expect("request B");
        let columns = refs(&body_b);
        assert!(columns.contains(&"b_date".to_string()), "join key is missing: {columns:?}");
        assert!(!columns.contains(&"a_date".to_string()), "foreign column leaked: {columns:?}");
        assert!(!columns.contains(&"a_val".to_string()), "foreign column leaked: {columns:?}");
    }

    #[test]
    fn template_fields_are_not_request_columns() {
        let config = json!({});
        let placeholders = vec![json!({"id": "y", "items": [
            {"guid": "m1", "title": "m1", "datasetId": "A", "type": "MEASURE", "data_type": "integer"},
            {"guid": "tpl", "title": "tpl", "type": "DIMENSION", "data_type": "string",
             "role_spec": {"role": "template", "template": ""}}
        ]})];
        let body = build_request_body(
            &config, &json!({}), &placeholders, &[], &[], &[], &[], &[], &[], &[], &[],
            "A", &[], &Value::Null, &json!({}),
        )
        .expect("request");
        let columns = refs(&body);
        assert_eq!(columns, vec!["m1"]);
    }

    #[test]
    fn merge_data_joins_linked_rows_and_appends_unlinked_ones() {
        let config = config_with_link();
        let first = response(
            "A",
            json!([
                {"id": "a_date", "title": "date", "data_type": "date", "legend_item_id": 1},
                {"id": "a_m", "title": "m", "data_type": "integer", "legend_item_id": 2}
            ]),
            json!([
                {"data": ["2020-01-01", 1], "legend": [1, 2]},
                {"data": ["2020-01-02", 2], "legend": [1, 2]}
            ]),
        );
        let second = response(
            "B",
            json!([
                {"id": "b_date", "title": "bdate", "data_type": "date", "legend_item_id": 1},
                {"id": "b_m", "title": "bm", "data_type": "integer", "legend_item_id": 2}
            ]),
            json!([
                {"data": ["2020-01-02", 10], "legend": [1, 2]},
                {"data": ["2020-01-03", 20], "legend": [1, 2]}
            ]),
        );
        let merged = merge_responses(&config, &[&first, &second]).expect("merge");
        let rows = merged
            .pointer("/result_data/0/rows")
            .and_then(Value::as_array)
            .expect("rows");
        assert_eq!(rows.len(), 3, "2 master rows + 1 unmatched row: {merged}");
        assert_eq!(rows[1]["data"], json!(["2020-01-02", 2, 10]), "linked row was not joined");
        assert_eq!(rows[2]["data"], json!(["2020-01-03", Value::Null, 20]), "unmatched row");
        // Both sides of the link share the merged column, other fields keep their own positions.
        let legend: Vec<i64> = merged
            .pointer("/fields")
            .and_then(Value::as_array)
            .expect("fields")
            .iter()
            .filter_map(|field| field.get("legend_item_id").and_then(Value::as_i64))
            .collect();
        assert_eq!(legend, vec![0, 1, 0, 2], "merged order positions: {merged}");
        assert_eq!(
            merged.pointer("/result_data/0/rows/0/legend"),
            Some(&json!([0, 1, 2]))
        );
    }

    #[test]
    fn merge_data_drops_the_template_placeholder_row() {
        let config = json!({});
        let fetched = response(
            "A",
            json!([
                {"id": "a_m", "title": "m", "data_type": "integer", "legend_item_id": 1},
                {"id": "tpl", "title": "tpl", "data_type": "string", "legend_item_id": 2,
                 "role_spec": {"role": "template", "template": ""}}
            ]),
            json!([
                {"data": [1, "x"], "legend": [1, 2]},
                {"data": [null, "x"], "legend": [2]}
            ]),
        );
        let merged = merge_responses(&config, &[&fetched]).expect("merge");
        let rows = merged
            .pointer("/result_data/0/rows")
            .and_then(Value::as_array)
            .expect("rows");
        assert_eq!(rows.len(), 1, "template placeholder row must be removed: {merged}");
        assert_eq!(rows[0]["data"], json!([1, "x"]));
    }

    #[test]
    fn merge_data_without_links_appends_unlinked_rows() {
        let config = json!({});
        let first = response(
            "A",
            json!([{"id": "a1", "title": "a1", "data_type": "string", "legend_item_id": 1}]),
            json!([{"data": ["a"], "legend": [1]}]),
        );
        let second = response(
            "B",
            json!([{"id": "b1", "title": "b1", "data_type": "string", "legend_item_id": 1}]),
            json!([{"data": ["b"], "legend": [1]}]),
        );
        let merged = merge_responses(&config, &[&first, &second]).expect("merge");
        let rows = merged
            .pointer("/result_data/0/rows")
            .and_then(Value::as_array)
            .expect("rows");
        // `mergeData` with no link between the datasets cannot join: the second dataset's
        // fields are unlinked and its row is appended with a null in the master column.
        assert_eq!(rows.len(), 1, "{merged}");
        assert_eq!(rows[0]["data"], json!(["a", "b"]));
    }
}
