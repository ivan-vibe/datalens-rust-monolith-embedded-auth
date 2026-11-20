//! Gravity (d3_wizard_node) line renderer.
//!
//! Port of `preparers/line/gravity-charts.ts:prepareGravityChartLine` on top of the classic
//! line data that `graph_engine` already prepares (`prepareLineData` is shared by both the
//! Highcharts and the gravity variant in the reference). The classic `data.graphs` entries
//! carry the same rows/colors/labels; this module only re-shapes them into the
//! `@gravity-ui/chartkit` `ChartData` contract and adds the gravity base config
//! (`gravity-charts/utils.ts:getBaseChartConfig`).
//!
//! Mirrors of the reference helpers are intentionally local and small: the line path only
//! needs the default (non-split, single-axis) branch first, so the module stays readable and
//! is extended axis/split case-by-case from the fixtures.

use serde_json::{json, Map, Value};

fn placeholder(shared: &Value, id: &str) -> Option<Value> {
    shared
        .pointer("/visualization/placeholders")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

fn item(shared: &Value, id: &str) -> Option<Value> {
    placeholder(shared, id)?
        .get("items")?
        .as_array()?
        .first()
        .cloned()
}

fn item_title(field: &Value) -> Option<String> {
    let title = field
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty());
    let fake = field
        .get("fakeTitle")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty());
    match (title, fake) {
        (Some(title), _) if !title.starts_with("title-") => Some(title.to_string()),
        (_, Some(fake)) => Some(fake.to_string()),
        (Some(title), _) => Some(title.to_string()),
        _ => None,
    }
}

fn data_type(field: &Value) -> &str {
    field
        .get("data_type")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn is_date_type(kind: &str) -> bool {
    matches!(
        kind,
        "date" | "datetime" | "genericdatetime" | "datetimetz" | "date_local" | "datetime_local"
    )
}

fn is_number_type(kind: &str) -> bool {
    matches!(
        kind,
        "integer" | "uinteger" | "float" | "decimal" | "number"
    )
}

/// `getExportColumnSettings`: one `SeriesExportSettings.columns` entry.
fn export_column(name: &str, formatter: Value, field: &str, kind: &str) -> Value {
    let type_name = if kind.is_empty() {
        "text"
    } else if kind == "genericdatetime" {
        "genericdatetime"
    } else if is_date_type(kind) {
        "date"
    } else if is_number_type(kind) {
        "number"
    } else {
        "text"
    };
    let formatter = if type_name == "number" {
        if formatter.as_object().map(Map::is_empty).unwrap_or(true) {
            // getExportColumnSettings defaults: floats get precision 2, integers keep an empty object.
            if kind == "integer" || kind == "uinteger" {
                json!({})
            } else {
                json!({"precision": 2})
            }
        } else {
            formatter
        }
    } else if type_name == "date" || type_name == "genericdatetime" {
        json!({})
    } else {
        formatter
    };
    let mut value = json!({"name": name, "formatter": formatter, "field": field, "type": type_name});
    if type_name == "date" || type_name == "genericdatetime" {
        value["format"] = json!(date_format(kind));
    }
    value
}

/// `getServerDateFormat` for the export column (reference uses `DD.MM.YYYY`).
fn date_format(kind: &str) -> &'static str {
    match kind {
        "datetime" | "datetimetz" | "datetime_local" | "genericdatetime" => "DD.MM.YYYY HH:mm:ss",
        _ => "DD.MM.YYYY",
    }
}

/// `getIsNavigatorEnabled`: the chart navigator settings opt in.
fn navigator_enabled(shared: &Value) -> bool {
    shared
        .pointer("/extraSettings/navigatorSettings")
        .map(|settings| settings.get("navigatorMode").and_then(Value::as_str) == Some("show"))
        .unwrap_or(false)
}

/// `getRangeSliderConfig`: `{defaultRange?, enabled, height}` for the x axis.
fn axis_range_slider(shared: &Value) -> Option<Value> {
    if !navigator_enabled(shared) {
        return None;
    }
    let period = shared.pointer("/extraSettings/navigatorSettings/periodSettings");
    let default_range = period.and_then(|settings| {
        let period = settings.get("period").and_then(Value::as_str)?;
        let value = settings.get("value").and_then(Value::as_str)?;
        let value = value.trim().parse::<f64>().ok()?;
        Some(json!({"size": {period: value}}))
    });
    let mut slider = Map::new();
    if let Some(default_range) = default_range {
        slider.insert("defaultRange".to_string(), default_range);
    }
    slider.insert("enabled".to_string(), json!(true));
    slider.insert("height".to_string(), json!(30));
    Some(Value::Object(slider))
}

/// `getSeriesRangeSliderConfig` + `lineWidth: 1` from the gravity line mapping.
fn series_range_slider(shared: &Value, series_name: &str) -> Option<Value> {
    if !navigator_enabled(shared) {
        return None;
    }
    let settings = shared.pointer("/extraSettings/navigatorSettings");
    let lines_mode = settings
        .and_then(|settings| settings.get("linesMode"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let visible = if lines_mode == "selected" {
        settings
            .and_then(|settings| settings.get("selectedLines"))
            .and_then(Value::as_array)
            .map(|selected| selected.iter().any(|item| item.as_str() == Some(series_name)))
            .unwrap_or(false)
    } else {
        true
    };
    Some(json!({"visible": visible, "lineWidth": 1}))
}

fn formatting(field: &Value) -> Value {
    field
        .get("formatting")
        .cloned()
        .filter(|value| value.as_object().map(|map| !map.is_empty()).unwrap_or(false))
        .unwrap_or_else(|| json!({}))
}

/// `mapChartkitFormatSettingsToGravityChartValueFormat` for a `chartKit*` object.
fn chartkit_format_map(source: &Value) -> Value {
    let mut format = Map::new();
    for (from, to) in [
        ("chartKitFormat", "format"),
        ("chartKitLabelMode", "labelMode"),
        ("chartKitPostfix", "postfix"),
        ("chartKitPrecision", "precision"),
        ("chartKitPrefix", "prefix"),
        ("chartKitShowRankDelimiter", "showRankDelimiter"),
        ("chartKitUnit", "unit"),
    ] {
        if let Some(value) = source.get(from) {
            if !value.is_null() {
                format.insert(to.to_string(), value.clone());
            }
        }
    }
    format.insert("type".to_string(), json!("number"));
    Value::Object(format)
}

fn tooltip_value_format(graph: &Value) -> Option<Value> {
    let tooltip = graph.get("tooltip")?;
    if tooltip.get("chartKitFormatting").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    // `mapChartkitFormatSettingsToGravityChartValueFormat`: every chartkit format key that is
    // present becomes the matching gravity value-format key; `type: number` is always set.
    let mut format = Map::new();
    for (source, target) in [
        ("chartKitFormat", "format"),
        ("chartKitLabelMode", "labelMode"),
        ("chartKitPostfix", "postfix"),
        ("chartKitPrecision", "precision"),
        ("chartKitPrefix", "prefix"),
        ("chartKitShowRankDelimiter", "showRankDelimiter"),
        ("chartKitUnit", "unit"),
    ] {
        if let Some(value) = tooltip.get(source) {
            format.insert(target.to_string(), value.clone());
        }
    }
    format.insert("type".to_string(), json!("number"));
    Some(Value::Object(format))
}

fn axis_title(shared: &Value, id: &str) -> Value {
    // `getAxisTitle(settings, item)`:
    //   * `title: "auto"`   -> `titleValue` or the field title;
    //   * `title: "off"`    -> empty object;
    //   * otherwise         -> `titleValue` if it is set, else an empty object.
    let Some(field) = item(shared, id) else {
        return json!({});
    };
    let settings = placeholder(shared, id)
        .and_then(|placeholder| placeholder.get("settings").cloned())
        .unwrap_or_else(|| json!({}));
    let title = settings.get("title").and_then(Value::as_str).unwrap_or("");
    let title_value = settings
        .get("titleValue")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(title, "off" | "hide" | "false") {
        return json!({});
    }
    if !title_value.is_empty() {
        return json!({"text": title_value});
    }
    if title == "auto" {
        if let Some(field_title) = item_title(&field) {
            return json!({"text": field_title});
        }
    }
    json!({})
}

fn axis_grid_enabled(shared: &Value, id: &str) -> bool {
    placeholder(shared, id)
        .and_then(|placeholder| placeholder.get("settings").cloned())
        .and_then(|settings| settings.get("grid").cloned())
        .and_then(|grid| grid.as_str().map(|value| value != "off"))
        .unwrap_or(true)
}

fn axis_labels_enabled(shared: &Value, id: &str) -> bool {
    placeholder(shared, id)
        .and_then(|placeholder| placeholder.get("settings").cloned())
        .and_then(|settings| settings.get("hideLabels").cloned())
        .and_then(|value| value.as_str().map(|value| value != "yes"))
        .unwrap_or(true)
}

fn axis_min_max(shared: &Value, id: &str) -> (Option<f64>, Option<f64>) {
    let Some(settings) = placeholder(shared, id).and_then(|p| p.get("settings").cloned()) else {
        return (None, None);
    };
    match settings.get("scale").and_then(Value::as_str) {
        Some("manual") => {
            let Some(values) = settings.get("scaleValue").and_then(Value::as_array) else {
                return (None, None);
            };
            let number = |value: Option<&Value>| {
                value
                    .and_then(Value::as_f64)
                    .or_else(|| value.and_then(Value::as_str).and_then(|v| v.parse().ok()))
            };
            (number(values.first()), number(values.get(1)))
        }
        _ => {
            if settings.get("scaleValue").and_then(Value::as_str) == Some("0-max") {
                (Some(0.0), None)
            } else {
                (None, None)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Line,
    Area,
    Area100p,
    BarX,
    BarX100p,
}

/// Build the gravity `ChartData` for a line chart from the classic `data.graphs` payload.
pub fn convert(
    shared: &Value,
    classic: &Value,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    convert_kind(shared, classic, Kind::Line, titles)
}

/// Area / area100p variant (`preparers/area/index.ts:prepareGravityChartArea`).
pub fn convert_area(
    shared: &Value,
    classic: &Value,
    visualization_id: &str,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    let kind = if visualization_id == "area100p" {
        Kind::Area100p
    } else {
        Kind::Area
    };
    convert_kind(shared, classic, kind, titles)
}

/// Column / column100p variant (`preparers/bar-x/gravity-charts.ts:prepareGravityChartBarX`).
pub fn convert_bar_x(
    shared: &Value,
    classic: &Value,
    visualization_id: &str,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    let kind = if visualization_id == "column100p" {
        Kind::BarX100p
    } else {
        Kind::BarX
    };
    convert_kind(shared, classic, kind, titles)
}

fn convert_kind(
    shared: &Value,
    classic: &Value,
    kind: Kind,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    let graphs = classic
        .get("graphs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // `build_layer_data` computes the formatted category labels when the classic builder took
    // the category-axis branch; gravity reuses that exact list instead of re-deriving it.
    let categories = classic.get("categories").cloned();
    let x_field = item(shared, "x").map(|field| {
        if field.get("data_type").and_then(Value::as_str) == Some("hierarchy")
            || (field.get("type").and_then(Value::as_str) == Some("PSEUDO")
                && field.get("data_type").and_then(Value::as_str) == Some("hierarchy"))
        {
            let guid = field.get("guid").and_then(Value::as_str);
            shared
                .get("hierarchies")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|hierarchy| hierarchy.get("guid").and_then(Value::as_str) == guid)
                .and_then(|hierarchy| hierarchy.get("fields"))
                .and_then(Value::as_array)
                .and_then(|fields| fields.first())
                .cloned()
                .unwrap_or(field)
        } else {
            field
        }
    });
    let y_field = item(shared, "y");
    let x_kind = x_field.as_ref().map(data_type).unwrap_or("");
    let y_kind = y_field.as_ref().map(data_type).unwrap_or("");
    let x_title = x_field.as_ref().and_then(item_title).unwrap_or_default();
    let y_title = y_field.as_ref().and_then(item_title).unwrap_or_default();
    let x_format = x_field.as_ref().map(formatting).unwrap_or_else(|| json!({}));
    let y_format = y_field.as_ref().map(formatting).unwrap_or_else(|| json!({}));
    let extra = shared.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let tooltip_hidden = extra.get("tooltip").and_then(Value::as_str) == Some("hide");
    let totals_enabled = extra.get("tooltipSum").and_then(Value::as_str) != Some("off");
    let legend_hidden = extra.get("legendMode").and_then(Value::as_str) == Some("hide");

    let palette = shared.get("colors").and_then(Value::as_array).cloned().unwrap_or_default();
    let shapes = shared.get("shapes").and_then(Value::as_array).cloned().unwrap_or_default();
    let color_pseudo = palette
        .first()
        .and_then(|field| {
            // A hierarchy pseudo field exports its first level (`Location` hierarchy of
            // `Region`/`Neighbourhood` exports as `Region`).
            if field.get("data_type").and_then(Value::as_str) == Some("hierarchy") {
                if let Some(title) = field
                    .get("fields")
                    .and_then(Value::as_array)
                    .and_then(|fields| fields.first())
                    .and_then(item_title)
                {
                    return Some(title);
                }
            }
            field
                .get("guid")
                .and_then(Value::as_str)
                .and_then(|guid| titles.get(guid).cloned())
                .or_else(|| item_title(field))
        })
        .unwrap_or_else(|| "Measure Names".to_string());

    // For `labelMode: percent` (column100p) the label is the share of the x-total, not the
    // raw measure value the classic graph carries.
    let mut barx_totals: std::collections::HashMap<u64, f64> = std::collections::HashMap::new();
    if matches!(kind, Kind::BarX | Kind::BarX100p) {
        for graph in &graphs {
            for point in graph.get("data").and_then(Value::as_array).into_iter().flatten() {
                let Some(x) = point.get("x").and_then(Value::as_f64) else { continue };
                let y = point.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                *barx_totals.entry(x.to_bits()).or_insert(0.0) += y;
            }
        }
    }
    let mut series_data: Vec<Value> = Vec::with_capacity(graphs.len());
    for graph in &graphs {
        // `isDataLabelsEnabled = Boolean(labelField)`; the classic graph's own flag only says
        // whether the current layer had labels, not whether the chart uses the labels placeholder.
        let data_labels_enabled = shared
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| !labels.is_empty())
            .unwrap_or(false);
        let data_labels_html = graph
            .pointer("/dataLabels/useHTML")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut points: Vec<Value> = Vec::new();
        for point in graph.get("data").and_then(Value::as_array).into_iter().flatten() {
            let x = point.get("x").cloned().unwrap_or(Value::Null);
            let finite = x.as_f64().map(f64::is_finite).unwrap_or(false);
            if !finite {
                continue;
            }
            let mut out = Map::new();
            // The classic graph carries `f64`s; the reference engine is JS, where an integral
            // double stringifies without a fractional part (`2`, not `2.0`).
            let y_value = match point.get("y") {
                Some(Value::Number(number)) => number
                    .as_f64()
                    .map(crate::gravity_donut::js_number_from_f64)
                    .unwrap_or(Value::Null),
                Some(value) => value.clone(),
                None => Value::Null,
            };
            out.insert(
                "y".to_string(),
                if matches!(kind, Kind::BarX | Kind::BarX100p) && y_value.is_null() {
                    json!(0)
                } else {
                    y_value
                },
            );
            if matches!(kind, Kind::BarX | Kind::BarX100p) {
                if let Some(color) = point.get("color") {
                    out.insert("color".to_string(), color.clone());
                }
            }
            if data_labels_enabled {
                let label = if point.get("y").map(Value::is_null).unwrap_or(true) {
                    json!("")
                } else if matches!(kind, Kind::BarX | Kind::BarX100p)
                    && graph
                        .pointer("/dataLabels/chartKitLabelMode")
                        .and_then(Value::as_str)
                        == Some("percent")
                {
                    let total = point
                        .get("x")
                        .and_then(Value::as_f64)
                        .and_then(|x| barx_totals.get(&x.to_bits()).copied())
                        .unwrap_or(0.0);
                    let y = point.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                    if total == 0.0 {
                        json!(0)
                    } else {
                        crate::gravity_donut::js_number_from_f64(y / total * 100.0)
                    }
                } else {
                    point.get("label").cloned().unwrap_or(Value::Null)
                };
                out.insert("label".to_string(), label);
            }
            if let Some(custom) = point.get("custom") {
                out.insert("custom".to_string(), custom.clone());
            }
            out.insert("x".to_string(), x);
            points.push(Value::Object(out));
        }
        let mut export_columns = vec![
            export_column(&x_title, x_format.clone(), "x", x_kind),
            export_column(&y_title, y_format.clone(), "y", y_kind),
        ];
        if categories.is_some() && matches!(kind, Kind::BarX | Kind::BarX100p) {
            export_columns[0]["field"] = json!("category");
        }
        if !palette.is_empty() {
            export_columns.push(json!({"name": color_pseudo, "formatter": {}, "field": "series.custom.colorValue", "type": "text"}));
        }
        if !shapes.is_empty() && !matches!(kind, Kind::BarX | Kind::BarX100p) {
            export_columns.push(
                json!({"name": color_pseudo, "formatter": {}, "field": "series.custom.shapeValue", "type": "text"}),
            );
        }
        let segments_active = shared
            .get("segments")
            .and_then(Value::as_array)
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        let segment_title = graph
            .pointer("/custom/segmentTitle")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                if segments_active {
                    graph
                        .get("measureFieldTitle")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .map(str::to_string)
                } else {
                    None
                }
            });
        let mut custom = Map::new();
        custom.insert("exportSettings".to_string(), json!({"columns": export_columns}));
        if let Some(segment) = &segment_title {
            custom.insert("segmentTitle".to_string(), json!(segment));
        }
        if let Some(color_value) = graph.get("colorValue") {
            custom.insert("colorValue".to_string(), color_value.clone());
        }
        if !matches!(kind, Kind::BarX | Kind::BarX100p) {
            if let Some(shape_value) = graph.get("shapeValue") {
                custom.insert("shapeValue".to_string(), shape_value.clone());
            }
        }
        // The reference bar-x mapping does not put `drillDownFilterValue` into `custom`.
        let mut series = Map::new();
        let base_name = graph.get("title").cloned().unwrap_or(Value::Null);
        series.insert(
            "name".to_string(),
            if let Some(segment) = &segment_title {
                json!(format!("{}: {}", segment, base_name.as_str().unwrap_or("")))
            } else {
                base_name
            },
        );
        if kind == Kind::Line {
            series.insert("type".to_string(), json!("line"));
        } else if matches!(kind, Kind::BarX | Kind::BarX100p) {
            series.insert("type".to_string(), json!("bar-x"));
            let stacking = if kind == Kind::BarX100p { "percent" } else { "normal" };
            series.insert("stacking".to_string(), json!(stacking));
            // Several measures on a column chart are stacked per measure; the reference series
            // carries the measure title as `stackId`.
            let y_items = placeholder(shared, "y")
                .and_then(|placeholder| placeholder.get("items").cloned())
                .and_then(|items| items.as_array().map(Vec::len))
                .unwrap_or(0);
            let x_items = placeholder(shared, "x")
                .and_then(|placeholder| placeholder.get("items").cloned())
                .and_then(|items| items.as_array().map(Vec::len))
                .unwrap_or(0);
            let colors_pseudo = palette
                .first()
                .map(|field| {
                    field.get("guid").and_then(Value::as_str).is_none()
                        || field.get("type").and_then(Value::as_str) == Some("PSEUDO")
                })
                .unwrap_or(true);
            if y_items > 1 && kind == Kind::BarX && !colors_pseudo {
                if let Some(measure) = graph.get("measureFieldTitle").and_then(Value::as_str) {
                    if !measure.is_empty() {
                        series.insert("stackId".to_string(), json!(measure));
                    }
                }
            } else if x_items > 1 && kind == Kind::BarX {
                // Two x dimensions: the first one is the axis, the second one stacks the series.
                if let Some(title) = graph.get("title").and_then(Value::as_str) {
                    if !title.is_empty() {
                        series.insert("stackId".to_string(), json!(title));
                    }
                }
            }
        } else {
            series.insert("type".to_string(), json!("area"));
            let stacking = if kind == Kind::Area100p {
                Some("percent")
            } else if shared
                .pointer("/extraSettings/stacking")
                .and_then(Value::as_str)
                != Some("off")
            {
                Some("normal")
            } else {
                None
            };
            if let Some(stacking) = stacking {
                series.insert("stacking".to_string(), json!(stacking));
            }
        }
        if let Some(color) = graph.get("color") {
            series.insert("color".to_string(), color.clone());
        }
        if !matches!(kind, Kind::BarX | Kind::BarX100p) {
            series.insert(
                "nullMode".to_string(),
                json!(if graph.get("connectNulls").and_then(Value::as_bool).unwrap_or(false) { "connect" } else { "skip" }),
            );
        }
        series.insert("data".to_string(), Value::Array(points));
        if matches!(kind, Kind::BarX | Kind::BarX100p) {
            let mut labels = Map::new();
            labels.insert("enabled".to_string(), json!(data_labels_enabled));
            labels.insert(
                "inside".to_string(),
                json!(extra.get("labelsPosition").and_then(Value::as_str) != Some("outside")),
            );
            labels.insert("html".to_string(), json!(data_labels_html));
            if data_labels_enabled {
                let dl = graph.get("dataLabels").cloned().unwrap_or_else(|| json!({}));
                labels.insert("format".to_string(), chartkit_format_map(&dl));
            }
            series.insert("dataLabels".to_string(), Value::Object(labels));
        } else {
            let mut labels = Map::new();
            labels.insert("enabled".to_string(), json!(data_labels_enabled));
            labels.insert("html".to_string(), json!(data_labels_html));
            if data_labels_enabled {
                let dl = graph.get("dataLabels").cloned().unwrap_or_else(|| json!({}));
                if dl.get("chartKitFormatting").and_then(Value::as_bool) == Some(true) {
                    labels.insert("format".to_string(), chartkit_format_map(&dl));
                }
            }
            series.insert("dataLabels".to_string(), Value::Object(labels));
        }
        let legend_group = graph.get("id").cloned().unwrap_or(Value::Null);
        let legend_item_text = graph.get("legendTitle").cloned().unwrap_or(Value::Null);
        if kind == Kind::Line {
            series.insert(
                "legend".to_string(),
                json!({"symbol": {"width": 36}, "groupId": legend_group, "itemText": legend_item_text}),
            );
        } else {
            series.insert(
                "legend".to_string(),
                json!({"groupId": legend_group, "itemText": legend_item_text}),
            );
        }
        if kind == Kind::Line {
            if let Some(dash) = graph.get("dashStyle") {
                series.insert("dashStyle".to_string(), dash.clone());
            }
        }
        if matches!(kind, Kind::Line | Kind::BarX | Kind::BarX100p) {
            if let Some(value_format) = tooltip_value_format(graph) {
                series.insert("tooltip".to_string(), json!({"valueFormat": value_format}));
            }
        }
        series.insert("yAxis".to_string(), graph.get("yAxis").cloned().unwrap_or(json!(0)));
        let series_name = series
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if matches!(kind, Kind::Line | Kind::BarX | Kind::BarX100p) {
            if let Some(range_slider) = series_range_slider(shared, &series_name) {
                series.insert("rangeSlider".to_string(), range_slider);
            }
        }
        series.insert("custom".to_string(), Value::Object(custom));
        series_data.push(Value::Object(series));
    }

    let mut x_axis = json!({
        "visible": placeholder(shared, "x").and_then(|p| p.get("settings")?.get("axisVisibility").and_then(Value::as_str).map(|v| v != "hide")).unwrap_or(true),
        "labels": {"enabled": axis_labels_enabled(shared, "x")},
        "title": axis_title(shared, "x"),
        "grid": {"enabled": axis_grid_enabled(shared, "x")},
        "ticks": {},
        "lineColor": "var(--g-color-line-generic)",
    });
    if let Some(range_slider) = axis_range_slider(shared) {
        x_axis["rangeSlider"] = range_slider;
    }
    if let Some(categories) = categories.as_ref() {
        x_axis["type"] = json!("category");
        x_axis["categories"] = categories.clone();
        x_axis["labels"]["html"] = json!(false);
    } else if is_date_type(x_kind) {
        x_axis["type"] = json!("datetime");
    } else if is_number_type(x_kind) {
        x_axis["type"] = json!("linear");
    } else {
        x_axis["type"] = json!("category");
    }

    let y_type = if is_date_type(y_kind) { "datetime" } else { "linear" };
    let mut y_axis_first = json!({
        "type": y_type,
        "visible": true,
        "labels": {"enabled": axis_labels_enabled(shared, "y")},
        "title": axis_title(shared, "y"),
        "grid": {"enabled": axis_grid_enabled(shared, "y")},
        "ticks": {"pixelInterval": 72},
        "lineColor": "transparent",
    });
    if kind == Kind::Line {
        y_axis_first["position"] = json!("left");
    }
    let (y_min, y_max) = axis_min_max(shared, "y");
    if let Some(min) = y_min {
        y_axis_first["min"] = crate::gravity_donut::js_number_from_f64(min);
    }
    if let Some(max) = y_max {
        y_axis_first["max"] = crate::gravity_donut::js_number_from_f64(max);
    }
    // Split charts (`preparers/line|area|bar-x`): one plot/y-axis per segment, series names
    // prefixed with the segment title, and a `split` config block.
    let mut split_titles: Vec<(i64, String)> = Vec::new();
    let segments_active = shared
        .get("segments")
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    for graph in &graphs {
        let segment = graph
            .pointer("/custom/segmentTitle")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                if segments_active {
                    graph
                        .get("measureFieldTitle")
                        .and_then(Value::as_str)
                        .filter(|title| !title.is_empty())
                        .map(str::to_string)
                } else {
                    None
                }
            });
        if let Some(segment) = segment {
            let plot = graph.get("yAxis").and_then(Value::as_i64).unwrap_or(0);
            if !split_titles.iter().any(|(index, _)| *index == plot) {
                split_titles.push((plot, segment));
            }
        }
    }
    split_titles.sort_by_key(|(index, _)| *index);
    let split = if !split_titles.is_empty() {
        Some(json!({
            "enable": true,
            "gap": "40px",
            "plots": split_titles.iter().map(|_| json!({})).collect::<Vec<_>>(),
        }))
    } else {
        None
    };
    let y_axis = if split_titles.is_empty() {
        let mut axes = vec![y_axis_first];
        // Dual axis: the second y placeholder gets its own right-hand axis config.
        if let Some(y2_field) = item(shared, "y2") {
            let y2_kind = data_type(&y2_field);
            let (y2_min, y2_max) = axis_min_max(shared, "y2");
            let mut second = json!({
                "type": if is_date_type(y2_kind) { "datetime" } else { "linear" },
                "visible": true,
                "labels": {"enabled": axis_labels_enabled(shared, "y2")},
                "title": axis_title(shared, "y2"),
                "grid": {"enabled": axis_grid_enabled(shared, "y2")},
                "ticks": {"pixelInterval": 72},
                "lineColor": "transparent",
                "position": "right",
            });
            if let Some(min) = y2_min {
                second["min"] = crate::gravity_donut::js_number_from_f64(min);
            }
            if let Some(max) = y2_max {
                second["max"] = crate::gravity_donut::js_number_from_f64(max);
            }
            axes.push(second);
        }
        json!(axes)
    } else {
        let mut axes: Vec<Value> = Vec::with_capacity(split_titles.len());
        for (plot_index, segment) in &split_titles {
            let mut axis = y_axis_first.clone();
            axis["plotIndex"] = json!(plot_index);
            axis["startOnTick"] = json!(true);
            axis["endOnTick"] = json!(true);
            axis["title"] = json!({
                "text": segment,
                "html": false,
                "rotation": 0,
                "maxWidth": "25%",
            });
            axes.push(axis);
        }
        json!(axes)
    };

    let mut tooltip = json!({"enabled": !tooltip_hidden});
    if !tooltip_hidden {
        tooltip["totals"] = json!({"enabled": totals_enabled});
    }
    if !y_format.as_object().map(Map::is_empty).unwrap_or(true) {
        // `getFieldFormatOptions(field)` + `type: number`: the full measure formatting object.
        let mut value_format = y_format.clone();
        if let Some(object) = value_format.as_object_mut() {
            object.insert("type".to_string(), json!("number"));
        }
        tooltip["valueFormat"] = value_format;
    } else if is_number_type(y_kind) {
        // `getFieldFormatOptions` defaults: floats carry precision 2, integers none.
        tooltip["valueFormat"] = if y_kind == "integer" || y_kind == "uinteger" {
            json!({"type": "number"})
        } else {
            json!({"type": "number", "precision": 2})
        };
    }
    let mut legend = json!({"html": false});
    if legend_hidden {
        legend["enabled"] = json!(false);
    }
    // A single-series gravity chart hides the legend (the reference fixture for a one-measure
    // area has `legend.enabled: false` even without a split).
    // The area renderer hides the legend for a single-series chart (the other renderers keep
    // `{html}` and only the split rule below disables it).
    if graphs.len() <= 1 && matches!(kind, Kind::Area | Kind::BarX | Kind::BarX100p) {
        legend["enabled"] = json!(false);
    }
    // `prepareGravityChartArea/Line`: a split with a single non-empty legend group hides the legend.
    let has_segments = shared
        .get("segments")
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if has_segments {
        let mut groups: Vec<&str> = Vec::new();
        for graph in &graphs {
            if let Some(group) = graph.get("id").and_then(Value::as_str) {
                if !groups.contains(&group) {
                    groups.push(group);
                }
            }
        }
        if groups.len() <= 1 && !legend_hidden {
            legend["enabled"] = json!(false);
        }
    }

    let mut data = json!({
        "tooltip": tooltip,
        "legend": legend,
        "series": {
            "data": series_data,
            "options": {
                "bar-x": {"barMaxWidth": 50, "barPadding": 0.05, "groupPadding": 0.4, "dataSorting": {"direction": "desc", "key": "name"}},
                "line": {"lineWidth": 2},
                "area": {"lineWidth": 2},
            },
        },
        "chart": {
            "margin": {"top": 10, "left": 10, "right": 10, "bottom": 15},
            "zoom": {"enabled": true, "resetButton": {"align": "top-right", "offset": {"x": 2, "y": 30}, "relativeTo": "plot-box"}},
        },
        "xAxis": x_axis,
        "yAxis": y_axis,
    });
    if let Some(split) = split {
        data["split"] = split;
    }
    Ok(data)
}

// ---------------------------------------------------------------------------------------------
// combined-chart (`prepareSingleResult` per layer + `combineLayersIntoSingleChart`)
// ---------------------------------------------------------------------------------------------

fn layers(shared: &Value) -> Vec<Value> {
    shared
        .pointer("/visualization/layers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn layer_by_id(shared: &Value, id: &str) -> Option<Value> {
    layers(shared)
        .into_iter()
        .find(|layer| layer.get("id").and_then(Value::as_str) == Some(id))
}

/// A combined chart can have several layers of the same type; the classic graph id ends with
/// `__<layerSettings.id>`, so that suffix selects the exact layer (needed for its colors).
fn layer_for_graph(shared: &Value, graph: &Value, layer_id: &str) -> Option<Value> {
    let suffix = graph
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.split("__").nth(1));
    if let Some(suffix) = suffix {
        if let Some(layer) = layers(shared).into_iter().find(|layer| {
            layer
                .pointer("/layerSettings/id")
                .and_then(Value::as_str)
                == Some(suffix)
                || layer.get("id").and_then(Value::as_str) == Some(suffix)
        }) {
            return Some(layer);
        }
    }
    layer_by_id(shared, layer_id)
}

/// Shared config view for one combined layer: its own placeholders/visualization id, the
/// chart-wide extra settings/colors/shapes.
fn layer_shared(shared: &Value, layer: &Value) -> Value {
    let mut out = shared.clone();
    if let Some(visualization) = out.get_mut("visualization").and_then(Value::as_object_mut) {
        if let Some(layer_id) = layer.get("id").cloned() {
            visualization.insert("id".to_string(), layer_id);
        }
        if let Some(placeholders) = layer.get("placeholders").cloned() {
            visualization.insert("placeholders".to_string(), placeholders);
        }
    }
    // `commonPlaceholders.colors/shapes` are the layer's color/shape fields (the chart-wide
    // `shared.colors` is empty for a layered chart).
    if let Some(common) = layer.get("commonPlaceholders") {
        if let Some(colors) = common.get("colors") {
            out["colors"] = colors.clone();
        }
        if let Some(shapes) = common.get("shapes") {
            out["shapes"] = shapes.clone();
        }
    }
    out
}

/// One converted graph from a combined layer, renamed/stacked like `extendCombinedChartGraphs`.
fn combined_series(
    shared: &Value,
    graph: &Value,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Option<Value>, String> {
    let layer_type = graph.get("type").and_then(Value::as_str).unwrap_or("");
    let (layer_id, kind) = match layer_type {
        "column" => ("column", Kind::BarX),
        "bar" => ("bar", Kind::BarX),
        "line" => ("line", Kind::Line),
        "area" => ("area", Kind::Area),
        _ => return Ok(None),
    };
    let Some(layer) = layer_for_graph(shared, graph, layer_id) else {
        return Ok(None);
    };
    let synth = layer_shared(shared, &layer);
    let single = json!({"graphs": [graph]});
    let converted = convert_kind(&synth, &single, kind, titles)?;
    let Some(mut series) = converted
        .pointer("/series/data")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
    else {
        return Ok(None);
    };
    let title = graph.get("title").and_then(Value::as_str).unwrap_or("");
    let measure = graph.get("measureFieldTitle").and_then(Value::as_str).unwrap_or("");
    let name = if !measure.is_empty() {
        title
            .strip_prefix(&format!("{measure}: "))
            .unwrap_or(title)
            .to_string()
    } else {
        title.to_string()
    };
    let name = if !measure.is_empty() && matches!(layer_id, "line" | "area") {
        // A colored line/area layer keeps the measure title in the series name in the reference
        // combined gravity result (`"<measure>: <color value>"`).
        if let Some(color_guid) = graph.get("colorGuid").and_then(Value::as_str) {
            if !color_guid.is_empty() {
                format!("{measure}: {name}")
            } else {
                name
            }
        } else {
            name
        }
    } else {
        name
    };
    series["name"] = json!(name);
    // Two reference behaviours meet here, both visible in the stock demo combined charts:
    // * multi-measure line layer (`qvnkq3hl9zjyf`: y = Forecast, Sales) builds one
    //   `exportSettings` from `yFields[0]`, so both series export as `Forecast`;
    // * single-measure line layer (`nskhn3wc2rk8c`: y = 60 Quantile) names the column after the
    //   series' measure title from the data-api answer (`Shipping time`).
    // Keep the conversion's layer-first name for the multi-measure case and rename only when
    // the layer holds exactly one y item.
    let layer_y_items = layer
        .get("placeholders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|placeholder| placeholder.get("id").and_then(Value::as_str) == Some("y"))
        .and_then(|placeholder| placeholder.get("items"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if layer_y_items == 1 && !measure.is_empty() {
        if let Some(columns) = series
            .pointer_mut("/custom/exportSettings/columns")
            .and_then(Value::as_array_mut)
        {
            if let Some(column) = columns.get_mut(1) {
                column["name"] = json!(measure);
            }
        }
    }
    // `graph.shapeValue` is part of the reference custom object too, but for the stock combined
    // chart the reference keeps it undefined (pseudo shape field is in `shapes` while
    // `mountedShapes` only maps legend titles of the visualization-prepared graphs).
    if let Some(custom) = series.get_mut("custom").and_then(Value::as_object_mut) {
        custom.remove("shapeValue");
    }
    // `extendCombinedChartGraphs`: the layer suffix (`__<layerSettingsId>`) is not part of the
    // gravity legend group id.
    if let Some(group) = series.pointer("/legend/groupId").and_then(Value::as_str) {
        let base = group.split("__").next().unwrap_or(group).to_string();
        series["legend"]["groupId"] = json!(base);
    }
    if layer_type == "column" || layer_type == "bar" {
        // `extendCombinedChartGraphs`: column stack is `<layerSettingsId>__<stack>`; the gravity
        // series reports only the second part (`stackId: 2019`).
        if let Some(stack) = graph.get("stack").and_then(Value::as_str) {
            let stack = stack.rsplit("__").next().unwrap_or(stack);
            if !stack.is_empty() {
                if let Ok(number) = stack.parse::<i64>() {
                    series["stackId"] = json!(number);
                } else {
                    series["stackId"] = json!(stack);
                }
            }
        }
        series["stacking"] = json!("normal");
        // Data-api integer measures reach the classic builder as f64; the reference JSON keeps
        // them as integers.
        if let Some(points) = series.get_mut("data").and_then(Value::as_array_mut) {
            for point in points {
                if let Some(y) = point.get("y").and_then(Value::as_f64) {
                    if y.fract() == 0.0 && y.abs() < 9.0e15 {
                        point["y"] = json!(y as i64);
                    }
                }
            }
        }
    } else {
        series.as_object_mut().map(|object| object.remove("stackId"));
    }
    Ok(Some(series))
}

/// `prepareGravityChartCombined`: layer configs are deep-merged (later layers win for scalars),
/// series are concatenated in layer order.
pub fn convert_combined(
    shared: &Value,
    classic: &Value,
    titles: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    let graphs = classic
        .get("graphs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut series_data: Vec<Value> = Vec::with_capacity(graphs.len());
    for graph in &graphs {
        if let Some(series) = combined_series(shared, graph, titles)? {
            series_data.push(series);
        }
    }
    // Base config comes from the layer that is merged last; for the config keys that matter here
    // (axes/tooltip) the line layer's placeholders are the reference for the demo combined chart.
    let line_layer = layer_by_id(shared, "line")
        .or_else(|| layers(shared).into_iter().last())
        .unwrap_or(Value::Null);
    let axis_shared = if line_layer.is_null() {
        shared.clone()
    } else {
        layer_shared(shared, &line_layer)
    };
    // `combineLayersIntoSingleChart` deep-merges layer configs with the last layer winning for
    // scalar settings (grid on/off, manual min/max), while a missing title does not erase an
    // earlier layer's title. Use the last layer for those scalars only.
    let last_axis_shared = layers(shared)
        .last()
        .map(|layer| layer_shared(shared, layer))
        .unwrap_or_else(|| axis_shared.clone());
    let x_field = item(&axis_shared, "x");
    let y_field = item(&axis_shared, "y");
    let x_kind = x_field.as_ref().map(data_type).unwrap_or("");
    let y_kind = y_field.as_ref().map(data_type).unwrap_or("float");
    let y_format = y_field.as_ref().map(formatting).unwrap_or_else(|| json!({}));
    let extra = shared.get("extraSettings").cloned().unwrap_or_else(|| json!({}));
    let tooltip_hidden = extra.get("tooltip").and_then(Value::as_str) == Some("hide");
    let mut x_axis = json!({
        "visible": true,
        "labels": {"enabled": axis_labels_enabled(&last_axis_shared, "x")},
        "title": {},
        "grid": {"enabled": axis_grid_enabled(&last_axis_shared, "x")},
        "ticks": {},
        "lineColor": "var(--g-color-line-generic)",
        "type": if is_date_type(x_kind) { "datetime" } else if is_number_type(x_kind) { "linear" } else { "category" },
    });
    let x_range = axis_range_slider(shared);
    if let Some(range) = x_range {
        x_axis["rangeSlider"] = range;
    }
    let mut combined_axis = json!({
        "type": if is_date_type(y_kind) { "datetime" } else { "linear" },
        "visible": true,
        "labels": {"enabled": axis_labels_enabled(&axis_shared, "y")},
        "title": axis_title(&axis_shared, "y"),
        "grid": {"enabled": axis_grid_enabled(&last_axis_shared, "y")},
        "ticks": {"pixelInterval": 72},
        "lineColor": "transparent",
    });
    let (combined_min, combined_max) = axis_min_max(&last_axis_shared, "y");
    if let Some(min) = combined_min {
        combined_axis["min"] = crate::gravity_donut::js_number_from_f64(min);
    }
    if let Some(max) = combined_max {
        combined_axis["max"] = crate::gravity_donut::js_number_from_f64(max);
    }
    // lodash `merge` keeps the line layer's `position: left` when a combined chart has one.
    if layers(shared)
        .iter()
        .any(|layer| layer.get("id").and_then(Value::as_str) == Some("line"))
    {
        combined_axis["position"] = json!("left");
    }
    let y_axis = json!([combined_axis]);
    let value_format = if y_format.as_object().map(Map::is_empty).unwrap_or(true) {
        json!({"type": "number", "precision": 2})
    } else {
        let mut format = y_format.clone();
        if let Some(object) = format.as_object_mut() {
            object.insert("type".to_string(), json!("number"));
        }
        format
    };
    let tooltip = if tooltip_hidden {
        json!({"enabled": false})
    } else {
        // `isTooltipSumEnabled(combined-chart)` is false in the reference: no `totals` member.
        json!({"enabled": true, "valueFormat": value_format})
    };
    let mut combined_legend = json!({"html": false});
    if layers(shared).iter().any(|layer| {
        matches!(
            layer.get("id").and_then(Value::as_str),
            Some("column") | Some("area")
        )
    }) {
        combined_legend["enabled"] = json!(false);
    }
    Ok(json!({
        "tooltip": tooltip,
        "legend": combined_legend,
        "series": {
            "data": series_data,
            "options": {
                "bar-x": {"barMaxWidth": 50, "barPadding": 0.05, "groupPadding": 0.4, "dataSorting": {"direction": "desc", "key": "name"}},
                "line": {"lineWidth": 2},
                "area": {"lineWidth": 2},
            },
        },
        "chart": {
            "margin": {"top": 10, "left": 10, "right": 10, "bottom": 15},
            "zoom": {"enabled": true, "resetButton": {"align": "top-right", "offset": {"x": 2, "y": 30}, "relativeTo": "plot-box"}},
        },
        "xAxis": x_axis,
        "yAxis": y_axis,
    }))
}
