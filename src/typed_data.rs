//! v5.3 typed chart data: the Rust data-api response, without the `serde_json::Value` hop.
//!
//! v5.2 introduced the direct in-process data-api path (`data_direct`), but it still
//! converted the typed `dl_api::ResultResponse` / `PivotResponse` into a JSON
//! `Value` and the chart builders then read that `Value` (and in several places
//! cloned it). The conversion allocates a second full copy of the result: one
//! map/array/string tree per chart.
//!
//! This module owns the shared typed representation:
//!
//! * [`DataBody`] — the result-level body (`Result` or `Pivot`), serializable as
//!   the wire JSON when a response has to be written;
//! * [`ResultBody`] — an owned `dl_api::ResultResponse` with borrowed accessors
//!   for the shapes the table / metric / graph builders read: legend items,
//!   `legend_item_id`-indexed fields, the first `result_data` chunk's rows
//!   (`data: Vec<Value>` + `legend: Vec<i64>`), blocks, notifications and
//!   `data_export`;
//! * the small enum→string mappings (`UserDataType`, `FieldType`,
//!   `dl_query::Role`, `Direction`) the JSON builders compared as strings.
//!
//! The builders receive `&ResultBody` and read typed fields; the response
//! envelope serializes once. No `Value` is created for the data-api body on the
//! `/result` chart paths (table, metric, graph). The `/pivot` path is the only
//! one that still needs the old JSON shape (`pivot_engine::build_data` is
//! written against it); [`DataBody::to_value`] is that documented bridge.

use std::fmt;

use dl_api::dto::BlockInfo;
use dl_api::{
    DataChunk, DataExport, DataRow, LegendItem, PivotResponse, ResultResponse, RoleSpec,
};
use dl_core::{FieldType, UserDataType};
use dl_query::{Direction, Role};
use serde::Serialize;
use serde_json::Value;

/// The wire-visible body of one chart source: a `/result` or a `/pivot` answer.
#[derive(Debug, Clone)]
pub enum DataBody {
    Result(ResultBody),
    Pivot(PivotResponse),
}

impl DataBody {
    /// The typed `/result` body, when this source is a `result` one.
    pub fn as_result(&self) -> Option<&ResultBody> {
        match self {
            Self::Result(body) => Some(body),
            Self::Pivot(_) => None,
        }
    }

    /// The typed `/pivot` body, when this source is a `pivot` one.
    pub fn as_pivot(&self) -> Option<&PivotResponse> {
        match self {
            Self::Result(_) => None,
            Self::Pivot(body) => Some(body),
        }
    }

    pub fn is_pivot(&self) -> bool {
        matches!(self, Self::Pivot(_))
    }

    /// The JSON body the old `Value`-based builders expect.
    ///
    /// This is the single deliberate bridge left (used by the pivot table
    /// engine and the standalone/HTTP fallback), so that a ported engine does
    /// not have to know about it.
    pub fn to_value(&self) -> Result<Value, String> {
        serde_json::to_value(self).map_err(|error| error.to_string())
    }

    /// Parse a wire JSON body (HTTP fallback / tests) into the typed body.
    pub fn from_value(value: Value) -> Result<Self, String> {
        if value.get("pivot_data").is_some() {
            serde_json::from_value::<PivotResponse>(value)
                .map(Self::Pivot)
                .map_err(|error| error.to_string())
        } else {
            serde_json::from_value::<ResultResponse>(value)
                .map(|response| Self::Result(ResultBody::new(response)))
                .map_err(|error| error.to_string())
        }
    }
}

impl Serialize for DataBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Result(body) => body.response.serialize(serializer),
            Self::Pivot(body) => body.serialize(serializer),
        }
    }
}

/// An owned `/result` response with borrowed accessors for the chart builders.
///
/// (`ResultResponse` is not `Default`, so this dead-until-v8.1 struct must not derive it.)
#[derive(Debug, Clone)]
pub struct ResultBody {
    pub response: ResultResponse,
}

impl ResultBody {
    pub fn new(response: ResultResponse) -> Self {
        Self { response }
    }

    /// The legend items (the response `fields` array).
    pub fn fields(&self) -> &[LegendItem] {
        &self.response.fields
    }

    /// The first `result_data` chunk's rows (the builder reads only the first chunk).
    pub fn rows(&self) -> &[DataRow] {
        self.response
            .result_data
            .first()
            .map(|chunk| chunk.rows.as_slice())
            .unwrap_or(&[])
    }

    pub fn rows_empty(&self) -> bool {
        self.rows().is_empty()
    }

    /// `legend`: the legend ids of the first row, i.e. the column order of `row.data`.
    pub fn legend(&self) -> &[i64] {
        self.rows()
            .first()
            .map(|row| row.legend.as_slice())
            .unwrap_or(&[])
    }

    pub fn chunks(&self) -> &[DataChunk] {
        &self.response.result_data
    }

    pub fn blocks(&self) -> &[BlockInfo] {
        &self.response.blocks
    }

    pub fn notifications(&self) -> &[Value] {
        &self.response.notifications
    }

    pub fn data_export(&self) -> &DataExport {
        &self.response.data_export
    }

    pub fn data_export_forbidden(&self) -> bool {
        self.response.data_export_forbidden
    }

    /// The legend item for a row value's `legend_item_id`.
    pub fn field_by_legend(&self, legend_item_id: i64) -> Option<&LegendItem> {
        self.fields()
            .iter()
            .find(|field| field.legend_item_id == legend_item_id)
    }

    /// The legend item for a field guid (`id` in the wire body) or title — the
    /// two lookups the builders do (fields are addressed by guid in requests,
    /// by title in a few legacy paths).
    pub fn field_by_id_or_title(&self, id_or_title: &str) -> Option<&LegendItem> {
        self.fields()
            .iter()
            .find(|field| field.id == id_or_title || field.title == id_or_title)
    }

    /// `legend_item_id -> row.data index` for the first chunk's column order.
    pub fn index_by_legend(&self) -> std::collections::HashMap<i64, usize> {
        let legend = self.legend();
        legend
            .iter()
            .enumerate()
            .map(|(index, legend_item_id)| (*legend_item_id, index))
            .collect()
    }

    /// `guid -> row.data index`, the map every builder builds from the legend.
    pub fn index_by_guid(&self) -> std::collections::HashMap<String, usize> {
        let legend = self.legend();
        let by_legend: std::collections::HashMap<i64, usize> = legend
            .iter()
            .enumerate()
            .map(|(index, legend_item_id)| (*legend_item_id, index))
            .collect();
        let mut map = std::collections::HashMap::with_capacity(self.fields().len());
        for field in self.fields() {
            if let Some(index) = by_legend.get(&field.legend_item_id) {
                map.entry(field.id.clone()).or_insert(*index);
            }
        }
        map
    }

    /// `guid -> data_type` (the string the JS/JSON builders used).
    pub fn data_types(&self) -> std::collections::HashMap<&str, &'static str> {
        self.fields()
            .iter()
            .map(|field| (field.id.as_str(), data_type_str(field.data_type)))
            .collect()
    }

    /// `legend_item_id -> title`.
    pub fn titles_by_legend(&self) -> std::collections::HashMap<i64, &str> {
        self.fields()
            .iter()
            .map(|field| (field.legend_item_id, field.title.as_str()))
            .collect()
    }
}

/// Consuming split of a `/result` body for the flat-table builder: the first chunk's rows
/// and the fields are **moved** out of the typed response instead of deep-cloned from a
/// `Value` tree. The rebuilt `Value` shapes are exactly the wire shapes the table builders
/// have always consumed.
pub struct TableParts {
    pub fields: Vec<Value>,
    pub rows: Vec<Value>,
    pub data_export: Value,
}

impl DataBody {
    /// `Some(parts)` for a `/result` body, `None` for pivot (not used by the flat table).
    ///
    /// `data_export` is handed in already serialized by the data-api reply: the typed
    /// `DataExport` is the same value on every path, so serializing it again here was a
    /// second redundant pass.
    pub fn into_table_parts(self, data_export: Value) -> Option<TableParts> {
        let Self::Result(body) = self else {
            return None;
        };
        let ResultResponse {
            fields,
            result_data,
            ..
        } = body.response;
        let fields: Vec<Value> = fields
            .into_iter()
            .filter_map(|field| serde_json::to_value(field).ok())
            .collect();
        let rows: Vec<Value> = result_data
            .into_iter()
            .next()
            .map(|chunk| {
                chunk
                    .rows
                    .into_iter()
                    .map(|row| {
                        // Move the cell `Value`s; no clone of the row batch.
                        let mut map = serde_json::Map::with_capacity(2);
                        map.insert("data".to_string(), Value::Array(row.data));
                        map.insert(
                            "legend".to_string(),
                            Value::Array(
                                row.legend
                                    .into_iter()
                                    .map(|id| Value::Number(id.into()))
                                    .collect(),
                            ),
                        );
                        Value::Object(map)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(TableParts {
            fields,
            rows,
            data_export,
        })
    }
}

impl DataBody {
    /// Rebuild the wire JSON `Value` of a `/result` body by **moving** the typed members
    /// (row cells included) instead of `serde_json::to_value` copying them. Used by the graph
    /// builders, which still read a `Value`; the envelope `size` comes from the typed
    /// serialization, so no JSON walk over the rebuilt tree is needed.
    pub fn into_result_wire_value(self, data_export: Value) -> Option<(Value, Value)> {
        let Self::Result(body) = self else {
            return None;
        };
        let ResultResponse {
            fields,
            result_data,
            blocks,
            notifications,
            data_export_forbidden,
            ..
        } = body.response;
        let fields: Value = Value::Array(
            fields
                .into_iter()
                .filter_map(|field| serde_json::to_value(field).ok())
                .collect(),
        );
        let result_data: Value = Value::Array(
            result_data
                .into_iter()
                .map(|chunk| {
                    let rows: Vec<Value> = chunk
                        .rows
                        .into_iter()
                        .map(|row| {
                            let mut map = serde_json::Map::with_capacity(2);
                            map.insert("data".to_string(), Value::Array(row.data));
                            map.insert(
                                "legend".to_string(),
                                Value::Array(
                                    row.legend
                                        .into_iter()
                                        .map(|id| Value::Number(id.into()))
                                        .collect(),
                                ),
                            );
                            Value::Object(map)
                        })
                        .collect();
                    let mut chunk_map = serde_json::Map::with_capacity(1);
                    chunk_map.insert("rows".to_string(), Value::Array(rows));
                    Value::Object(chunk_map)
                })
                .collect(),
        );
        let blocks: Value = Value::Array(
            blocks
                .into_iter()
                .filter_map(|block| serde_json::to_value(block).ok())
                .collect(),
        );
        let mut body_map = serde_json::Map::with_capacity(6);
        body_map.insert("fields".to_string(), fields);
        body_map.insert("result_data".to_string(), result_data);
        body_map.insert("blocks".to_string(), blocks);
        body_map.insert("notifications".to_string(), Value::Array(notifications));
        body_map.insert("data_export".to_string(), data_export.clone());
        body_map.insert(
            "data_export_forbidden".to_string(),
            Value::Bool(data_export_forbidden),
        );
        Some((Value::Object(body_map), data_export))
    }
}

impl Serialize for ResultBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.response.serialize(serializer)
    }
}

// ---------------------------------------------------------------------------
// Enum -> string mappings
// ---------------------------------------------------------------------------

/// `UserDataType` serialized as the snake_case wire string (`"genericdatetime"`).
pub fn data_type_str(data_type: UserDataType) -> &'static str {
    match data_type {
        UserDataType::String => "string",
        UserDataType::Integer => "integer",
        UserDataType::Float => "float",
        UserDataType::Date => "date",
        UserDataType::Datetime => "datetime",
        UserDataType::Boolean => "boolean",
        UserDataType::Geopoint => "geopoint",
        UserDataType::Geopolygon => "geopolygon",
        UserDataType::Uuid => "uuid",
        UserDataType::Markup => "markup",
        UserDataType::Datetimetz => "datetimetz",
        UserDataType::Unsupported => "unsupported",
        UserDataType::ArrayStr => "array_str",
        UserDataType::ArrayInt => "array_int",
        UserDataType::ArrayFloat => "array_float",
        UserDataType::TreeStr => "tree_str",
        UserDataType::Genericdatetime => "genericdatetime",
    }
}

/// `FieldType` serialized as the wire string (`"DIMENSION"` / `"MEASURE"`).
pub fn field_type_str(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::Dimension => "DIMENSION",
        FieldType::Measure => "MEASURE",
    }
}

/// `FieldRole`/`Role` serialized as the lowercase wire string.
pub fn role_str(role: Role) -> &'static str {
    role.as_str()
}

/// The role of a `role_spec`, or `""` when absent.
pub fn role_of(role_spec: &RoleSpec) -> &'static str {
    role_str(role_spec.role)
}

/// `Direction` serialized as the lowercase wire string.
pub fn direction_str(direction: Direction) -> &'static str {
    match direction {
        Direction::Asc => "asc",
        Direction::Desc => "desc",
    }
}

/// One field description as the builders read it (`s(field, "...")` replacements).
#[derive(Clone, Copy)]
pub struct FieldView<'a> {
    pub field: &'a LegendItem,
}

impl<'a> FieldView<'a> {
    pub fn new(field: &'a LegendItem) -> Self {
        Self { field }
    }
    pub fn id(&self) -> &'a str {
        &self.field.id
    }
    pub fn title(&self) -> &'a str {
        &self.field.title
    }
    pub fn legend_item_id(&self) -> i64 {
        self.field.legend_item_id
    }
    pub fn data_type(&self) -> &'static str {
        data_type_str(self.field.data_type)
    }
    pub fn field_type(&self) -> &'static str {
        field_type_str(self.field.field_type)
    }
    pub fn item_type(&self) -> &'a str {
        &self.field.item_type
    }
    pub fn role(&self) -> &'static str {
        role_of(&self.field.role_spec)
    }
    pub fn role_spec(&self) -> &'a RoleSpec {
        &self.field.role_spec
    }
}

impl fmt::Debug for FieldView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FieldView")
            .field("id", &self.field.id)
            .field("title", &self.field.title)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dl_api::{DataChunk, DataExport, ExportPermission, ResultResponse};

    fn export() -> DataExport {
        DataExport {
            background: ExportPermission {
                allowed: false,
                reason: Some(vec!["PROHIBITED_EXPORT_TENANT".to_string()]),
            },
            basic: ExportPermission {
                allowed: true,
                reason: None,
            },
        }
    }

    fn response() -> ResultResponse {
        serde_json::from_value(serde_json::json!({
            "fields": [
                {
                    "legend_item_id": 0,
                    "id": "dim-1",
                    "title": "FirstOrder_Month",
                    "data_type": "date",
                    "field_type": "DIMENSION",
                    "item_type": "field",
                    "role_spec": {"role": "row"}
                },
                {
                    "legend_item_id": 1,
                    "id": "msr-1",
                    "title": "COUNTS",
                    "data_type": "integer",
                    "field_type": "MEASURE",
                    "item_type": "field",
                    "role_spec": {"role": "measure"}
                }
            ],
            "result_data": [{"rows": [
                {"data": ["2019-05-01", "7"], "legend": [0, 1]},
                {"data": ["2019-06-01", "9"], "legend": [0, 1]}
            ]}],
            "blocks": [{"block_id": 0, "query": "select 1"}],
            "notifications": [],
            "data_export": export(),
            "data_export_forbidden": false
        }))
        .expect("result response")
    }

    #[test]
    fn typed_accessors_match_the_wire_shape() {
        let body = ResultBody::new(response());
        assert_eq!(body.fields().len(), 2);
        assert_eq!(body.rows().len(), 2);
        assert_eq!(body.legend(), &[0, 1]);
        assert_eq!(body.legend()[1], 1);
        assert_eq!(body.field_by_legend(1).map(|field| field.title.as_str()), Some("COUNTS"));
        assert_eq!(
            body.field_by_id_or_title("COUNTS").map(|field| field.id.as_str()),
            Some("msr-1")
        );
        assert_eq!(body.index_by_guid().get("msr-1"), Some(&1));
        assert_eq!(body.data_types().get("msr-1"), Some(&"integer"));
        assert_eq!(body.rows()[0].data[0], serde_json::json!("2019-05-01"));
        assert_eq!(body.blocks()[0].block_id, 0);
        assert_eq!(body.data_export().basic.allowed, true);
        assert!(!body.data_export_forbidden());
        assert_eq!(data_type_str(body.fields()[0].data_type), "date");
        assert_eq!(field_type_str(body.fields()[1].field_type), "MEASURE");
        assert_eq!(role_of(&body.fields()[1].role_spec), "measure");
    }

    #[test]
    fn wire_value_roundtrip_is_stable() {
        let value = serde_json::json!({
            "fields": [{
                "legend_item_id": 0,
                "id": "f",
                "title": "F",
                "data_type": "string",
                "field_type": "DIMENSION",
                "item_type": "field",
                "role_spec": {"role": "row"}
            }],
            "result_data": [],
            "blocks": [],
            "notifications": [],
            "data_export": export(),
            "data_export_forbidden": true
        });
        let body = DataBody::from_value(value.clone()).expect("typed body");
        assert!(!body.is_pivot());
        assert_eq!(body.to_value().expect("value"), value);
        let pivot = serde_json::json!({
            "fields": [],
            "pivot": {"structure": []},
            "pivot_data": {
                "columns": [], "columns_with_info": [],
                "row_dimension_headers": [], "rows": []
            },
            "blocks": [], "notifications": [], "data_export": export()
        });
        let body = DataBody::from_value(pivot.clone()).expect("pivot body");
        assert!(body.is_pivot());
        assert_eq!(body.to_value().expect("value"), pivot);
    }

    /// The data-api measures `sources[].size` once, by counting the compact JSON bytes of the
    /// body instead of serializing it to text. The count must be exactly what the reference
    /// serializer would write — including escaping, unicode and number formatting.
    #[test]
    fn measured_json_len_equals_the_serialized_body_length() {
        let mut response = response();
        // Cells the engine really sees: plain strings, a string full of escapable characters,
        // unicode that serde leaves as UTF-8, numeric cells and a markup object.
        response.result_data = vec![DataChunk {
            rows: vec![
                DataRow {
                    data: vec![
                        serde_json::json!("2019-05-01"),
                        serde_json::json!("quote \" backslash \\ newline \n tab \t"),
                        serde_json::json!("Москва 🙂"),
                        serde_json::json!(42),
                        serde_json::json!(10.5),
                        serde_json::json!({"type": "text", "value": "a\"b"}),
                    ],
                    legend: vec![0, 1, 2, 3, 4, 5],
                },
                DataRow {
                    data: vec![
                        serde_json::json!(""),
                        serde_json::json!("\u{1}control"),
                        serde_json::json!("ends-with-a-quote\""),
                        serde_json::json!(-7),
                        serde_json::json!(1e300),
                        serde_json::json!([]),
                    ],
                    legend: vec![0, 1, 2, 3, 4, 5],
                },
            ],
        }];
        response.measure_json_len();
        let serialized = serde_json::to_string(&response).expect("serialize");
        assert_eq!(response.json_len, serialized.len() as u64);
        // The measurement is not part of the wire body.
        assert!(!serialized.contains("json_len"));
    }

    #[test]
    fn empty_result_has_no_rows_and_no_legend() {
        let mut response = response();
        response.result_data = vec![DataChunk { rows: vec![] }];
        // `ResultBody::new` takes ownership; clone for the first body so the second
        // assertion below can still reuse the response.
        let body = ResultBody::new(response.clone());
        assert!(body.rows_empty());
        assert_eq!(body.legend(), &[] as &[i64]);
        assert!(body.index_by_guid().is_empty());
        let response = {
            let mut response = response;
            response.result_data.clear();
            response
        };
        let body = ResultBody::new(response);
        assert!(body.rows_empty());
    }
}
