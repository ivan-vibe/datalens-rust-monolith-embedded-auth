//! The v2 `data-api` JSON contract.
//!
//! Field names follow `dl_api_lib.schemas.data` exactly (`fields`, `order_by`, `filters`,
//! `updates`, `row_count_hard_limit`, `result_data`, `blocks`, ...), so an existing client
//! can point at the Rust service without a translation layer.

use dl_core::{Dataset, FieldRef, UserDataType};
use dl_query::{Direction, FilterOperation, FilterSpec, OrderBySpec, ParameterValueSpec, Role};
use serde::{Deserialize, Serialize, Serializer};

/// Exact compact-JSON byte length of a serializable value.
///
/// The engine reports `sources[].size` — the number of bytes the reference service's JSON
/// serializer would write for the response body — but it never needs the text itself. Counting
/// the bytes through [`std::io::Write`] gives the exact figure in one pass, with no buffer, no
/// allocation and no intermediate `String`: the value tree is walked, the bytes are discarded.
pub fn json_len<T: ?Sized + Serialize>(value: &T) -> u64 {
    let mut count = CountWriter(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(_) => 0,
    }
}

/// A writer that only counts bytes: no buffer, no allocation, no UTF-8 validation.
#[derive(Default)]
struct CountWriter(u64);

impl std::io::Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::fmt::Write for CountWriter {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0 += text.len() as u64;
        Ok(())
    }
}

/// Compact-JSON length of a string value, escaping exactly like `serde_json` does.
fn string_json_len(text: &str) -> u64 {
    let mut len = 2; // the quotes
    for byte in text.as_bytes() {
        len += match *byte {
            b'"' | b'\\' => 2,
            // `serde_json` writes these five control characters as a two-byte escape.
            0x08 | 0x09 | 0x0a | 0x0c | 0x0d => 2,
            // Every other C0 control character becomes a six-byte `\u00XX` escape.
            byte if byte < 0x20 => 6,
            _ => 1,
        };
    }
    len
}

/// Compact-JSON length of any JSON value, without rendering it.
pub fn value_json_len(value: &serde_json::Value) -> u64 {
    use serde_json::Value;
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => {
            let mut count = CountWriter(0);
            match serde_json::to_writer(&mut count, number) {
                Ok(()) => count.0,
                Err(_) => 0,
            }
        }
        Value::String(text) => string_json_len(text),
        Value::Array(items) => {
            let mut len = 2 + items.len().saturating_sub(1) as u64; // brackets + commas
            for item in items {
                len += value_json_len(item);
            }
            len
        }
        Value::Object(map) => {
            let mut len = 2 + map.len().saturating_sub(1) as u64; // braces + commas
            for (key, item) in map {
                len += string_json_len(key) + 1 + value_json_len(item);
            }
            len
        }
    }
}

/// `{"data": [...], "legend": [...]}` compact-JSON length of one result row.
pub fn data_row_json_len(row: &DataRow) -> u64 {
    // `{"data":` (8) + `[...]` + `,"legend":` (10) + `[...]` + `}` (1)
    let mut len = 19;
    len += 2 + row.data.len().saturating_sub(1) as u64;
    for cell in &row.data {
        len += value_json_len(cell);
    }
    len += 2 + row.legend.len().saturating_sub(1) as u64;
    for id in &row.legend {
        let mut count = CountWriter(0);
        let _ = std::fmt::Write::write_fmt(&mut count, format_args!("{id}"));
        len += count.0;
    }
    len
}

/// `{"rows": [...]}` compact-JSON length of one result chunk.
fn data_chunk_json_len(chunk: &DataChunk) -> u64 {
    // `{"rows":` (8) + `[` (1) + `]}` (2) + one comma between the rows
    let mut len = 11 + chunk.rows.len().saturating_sub(1) as u64;
    for row in &chunk.rows {
        len += data_row_json_len(row);
    }
    len
}

/// Compact-JSON length of a `ResultResponse`, the exact value `serde_json::to_string` would
/// write. The row cells are counted directly; the small typed members (fields, blocks,
/// notifications, data_export) go through the counting serializer.
pub fn result_json_len(response: &ResultResponse) -> u64 {
    fn key(name: &str) -> u64 {
        // `"name":` — two quotes and the colon
        name.len() as u64 + 3
    }
    let mut len = 2; // the object braces
    len += key("fields") + json_len(&response.fields);
    len += 1; // comma
    len += key("result_data") + 2 + response.result_data.len().saturating_sub(1) as u64;
    for chunk in &response.result_data {
        len += data_chunk_json_len(chunk);
    }
    len += 1; // comma
    len += key("blocks") + json_len(&response.blocks);
    len += 1;
    len += key("notifications") + json_len(&response.notifications);
    len += 1;
    len += key("data_export") + json_len(&response.data_export);
    len += 1;
    len += key("data_export_forbidden") + if response.data_export_forbidden { 4 } else { 5 };
    len
}



/// Request integers, read the way pydantic reads them.
///
/// The schemas declare `limit`, `offset`, `row_count_hard_limit`, `legend_item_id`, `block_id`
/// and `level` as integers, but pydantic coerces a numeric *string* to an `int` — and the
/// editor's "Number of rows" input posts exactly that (`"limit": "100"`, the value it shows).
/// Reading only JSON numbers turned a valid preview into `422 Unprocessable Entity`, so both
/// spellings are accepted here. Responses keep writing plain numbers: only the readers are
/// lenient.
pub mod lenient_int {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// A required integer: `100`, `"100"` and `"100.0"` all read as `100`.
    pub fn required<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        let value = Value::deserialize(deserializer)?;
        number(&value).ok_or_else(|| D::Error::custom(format!("expected an integer, got {value}")))
    }

    /// An optional integer: `null`, a missing key and an empty string all mean "no value".
    pub fn optional<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match &value {
            Value::Null => Ok(None),
            Value::String(text) if text.trim().is_empty() => Ok(None),
            _ => number(&value)
                .map(Some)
                .ok_or_else(|| D::Error::custom(format!("expected an integer, got {value}"))),
        }
    }

    /// `pydantic`'s coercion: numbers are truncated toward zero, strings are parsed.
    fn number(value: &Value) -> Option<i64> {
        match value {
            Value::Number(number) => number.as_i64().or_else(|| {
                let float = number.as_f64()?;
                float.is_finite().then_some(float.trunc() as i64)
            }),
            Value::String(text) => {
                let text = text.trim();
                text.parse::<i64>().ok().or_else(|| {
                    let float = text.parse::<f64>().ok()?;
                    float.is_finite().then_some(float.trunc() as i64)
                })
            }
            _ => None,
        }
    }
}

/// `POST /api/data/v2/datasets/{ds_id}/result`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResultRequest {
    #[serde(default)]
    pub fields: Vec<FieldsItem>,
    #[serde(default)]
    pub order_by: Vec<OrderByItem>,
    #[serde(default)]
    pub filters: Vec<FilterItem>,
    #[serde(default)]
    pub result: Option<ResultSpec>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub offset: Option<i64>,
    #[serde(default)]
    pub disable_group_by: bool,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub row_count_hard_limit: Option<i64>,
    #[serde(default)]
    pub updates: Vec<UpdateAction>,
    #[serde(default)]
    pub autofill_legend: bool,
    /// `parameter_values`: values of the dataset parameters, overriding `default_value`.
    #[serde(default)]
    pub parameter_values: Vec<ParameterValueItem>,
    /// A filter over a field that no longer exists is dropped instead of rejected.
    #[serde(default)]
    pub ignore_nonexistent_filters: bool,
    /// The pinned dataset revision. The charts engine (monolith) sends it under the Python
    /// v2 name `dataset_revision_id`; accepting both spellings is what lets the data-api pin
    /// the dataset and gives the query-result cache key a revision to invalidate on.
    #[serde(default, alias = "dataset_revision_id")]
    pub revision_id: Option<String>,
    /// Inline dataset, as `/datasets/data/preview` sends it.
    #[serde(default)]
    pub dataset: Option<Dataset>,
}

impl ResultRequest {
    /// `result.with_totals` (`RequestResultSpecSchema`).
    ///
    /// The v2 request nests it in `result`; v1 put it at the top level, which is why the flag
    /// lives in its own object in `dl_api_lib.schemas.data` too. The v1/v1.5 handler translates
    /// its top-level flag into this object.
    pub fn with_totals(&self) -> bool {
        self.result
            .as_ref()
            .is_some_and(|result| result.with_totals)
    }
}

/// `RequestResultSpecSchema` (`RawResultSpec`): the `result` object of a v2 request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSpec {
    #[serde(default)]
    pub with_totals: bool,
}

/// `POST /api/data/v2/datasets/data/preview`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PreviewRequest {
    #[serde(default)]
    pub dataset: Option<Dataset>,
    #[serde(default)]
    pub updates: Vec<UpdateAction>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub row_count_hard_limit: Option<i64>,
}

/// `ActionSchema`: only the actions `data-api` accepts are modelled (see [`crate::updates`]).
pub use crate::updates::{DeletedField, FieldPatch, UpdateAction};

/// `FieldVisibility`: used by `row`/`template` role specs (pivot visibility, ignored here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
}

/// `dl_constants.RangeType`: the two ends `/values/range` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RangeType {
    Min,
    Max,
}

/// `dl_api_lib.schemas.data.RoleSpecSchema`: the `role_spec` object of one legend item.
///
/// The API keeps the role and its parameters in one nested object, keyed by `role`:
/// `{"role": "row"}`, `{"role": "order_by", "direction": "desc"}`,
/// `{"role": "template", "template": "{{f_region}}", "visibility": "visible"}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleSpec {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    /// `RawTreeRoleSpec.level`: how many leading entries of the tree the `tree` role shows.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_int::optional"
    )]
    pub level: Option<i64>,
    /// `RawTreeRoleSpec.prefix`: a JSON *string* in the request (`"[]"`) and an *array* in the
    /// response, which is why it is kept as raw JSON and normalized by the legend formalizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<serde_json::Value>,
    /// `RawTreeRoleSpec.dimension_values`: the locked dimensions of a tree level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension_values: Option<serde_json::Value>,
    /// `RangeRoleSpec.range_type`: which end of the range a `range` legend item is.
    ///
    /// `/values/range` takes two items of the same field (`min` and `max`) and echoes the whole
    /// `role_spec` back, so the key has to survive the round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_type: Option<RangeType>,
    /// `FilterRoleSpec.operation`: only the filter items `_generate_tree_filter_items` builds
    /// carry it, and `RoleSpecSchema` dumps a filter item as `{"role": "filter"}`, so it never
    /// reaches a response — it exists so the compiler can run the very filters the legend
    /// announces.
    #[serde(default, skip_serializing)]
    pub operation: Option<FilterOperation>,
    /// `FilterRoleSpec.values`: see [`RoleSpec::operation`].
    #[serde(default, skip_serializing)]
    pub values: Option<Vec<serde_json::Value>>,
}

/// `RawDimensionValueSpec`: one dimension a tree level is locked by.
///
/// The value is the *postprocessed* one (`PostprocessedValue`), i.e. the string the client saw
/// in the parent row (`"[\"Electronics\"]"` for a tree), not the raw array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimensionValueSpec {
    #[serde(deserialize_with = "lenient_int::required")]
    pub legend_item_id: i64,
    pub value: serde_json::Value,
}

impl RoleSpec {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            ..Self::default()
        }
    }

    /// `TreeRoleSpec.dimension_values`, parsed: the request sends them as an array of
    /// `{legend_item_id, value}` objects, and `_generate_tree_filter_items` needs the ids.
    pub fn tree_dimension_values(&self) -> Vec<DimensionValueSpec> {
        self.dimension_values
            .as_ref()
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }

    /// `TreeRoleSpec.prefix` as a list: a level of the tree is everything below its ancestors.
    pub fn tree_prefix(&self) -> Vec<serde_json::Value> {
        self.prefix
            .as_ref()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
    }

    /// `RowRoleSpec`: a dimension, `FieldVisibility.visible` unless the client says otherwise.
    pub fn row() -> Self {
        Self {
            role: Role::Row,
            visibility: Some(Visibility::Visible),
            ..Self::default()
        }
    }

    /// `TemplateRoleSpec`: the value of a `template` item is the `template` string itself.
    pub fn template(template: impl Into<String>) -> Self {
        Self {
            role: Role::Template,
            template: Some(template.into()),
            visibility: Some(Visibility::Visible),
            ..Self::default()
        }
    }
}

impl From<Role> for RoleSpec {
    fn from(role: Role) -> Self {
        Self::new(role)
    }
}

/// `QueryFieldsItemSchema`: one `fields` item.
///
/// The API sends the role nested in `role_spec`. The flat `role` this port accepted before
/// M2b is still read (and `dimension` is still an alias of `row`) so old requests keep working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldsItem {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    #[serde(default)]
    pub role_spec: Option<RoleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// `null` unless the client pins the legend ids (the usual case).
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub legend_item_id: Option<i64>,
    /// `null` for the main block. The port compiles one block, so only `0`/`null` is accepted.
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub block_id: Option<i64>,
    /// Pivot label; carried through the request model and ignored by `/result`.
    #[serde(default)]
    pub label: Option<String>,
}

impl FieldsItem {
    pub fn new(field_ref: FieldRef, role: Role) -> Self {
        Self {
            field_ref,
            role_spec: Some(RoleSpec::new(role)),
            role: None,
            legend_item_id: None,
            block_id: None,
            label: None,
        }
    }

    /// The role of the item: `role_spec.role`, else the legacy flat `role`, else `row`.
    pub fn role(&self) -> Role {
        self.role_spec
            .as_ref()
            .map(|role_spec| role_spec.role)
            .or(self.role)
            .unwrap_or_default()
    }

    /// `TemplateRoleSpec.template`, when the item is a template.
    pub fn template(&self) -> Option<&str> {
        self.role_spec
            .as_ref()
            .and_then(|role_spec| role_spec.template.as_deref())
    }

    /// The item as the compiler sees it (the legend keeps the ids, not the spec).
    pub fn to_select(&self) -> dl_query::SelectItem {
        dl_query::SelectItem {
            field_ref: self.field_ref.clone(),
            role: self.role(),
            level: self
                .role_spec
                .as_ref()
                .and_then(|role_spec| role_spec.level),
        }
    }
}

/// `QueryOrderByItemSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderByItem {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub block_id: Option<i64>,
}

impl OrderByItem {
    pub fn new(field_ref: FieldRef, direction: Direction) -> Self {
        Self {
            field_ref,
            direction,
            block_id: None,
        }
    }
}

impl From<&OrderByItem> for OrderBySpec {
    fn from(item: &OrderByItem) -> Self {
        Self {
            field_ref: item.field_ref.clone(),
            direction: item.direction,
        }
    }
}

/// `QueryFiltersItemSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterItem {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    pub operation: FilterOperation,
    #[serde(default)]
    pub values: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub block_id: Option<i64>,
}

impl FilterItem {
    pub fn new(
        field_ref: FieldRef,
        operation: FilterOperation,
        values: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            field_ref,
            operation,
            values,
            block_id: None,
        }
    }
}

impl From<&FilterItem> for FilterSpec {
    fn from(item: &FilterItem) -> Self {
        Self {
            field_ref: item.field_ref.clone(),
            operation: item.operation,
            values: item.values.clone(),
        }
    }
}

/// `ParameterValueSchema`: a parameter is a constant for one query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterValueItem {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    pub value: serde_json::Value,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub block_id: Option<i64>,
}

impl From<&ParameterValueItem> for ParameterValueSpec {
    fn from(item: &ParameterValueItem) -> Self {
        Self {
            field_ref: item.field_ref.clone(),
            value: item.value.clone(),
        }
    }
}

/// `LegendItemSchema`: one entry of the response `fields` ("legend" in `dl_query_processing`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegendItem {
    pub legend_item_id: i64,
    pub id: String,
    pub title: String,
    pub data_type: UserDataType,
    pub field_type: FieldType,
    pub item_type: String,
    /// `role_spec.role` and its parameters: what tells the client that an item is a measure,
    /// a filter or an `info` field that has no value in the rows.
    pub role_spec: RoleSpec,
}

/// `dl_constants.enums.FieldType` — defined in `dl_core` because datasets carry it.
pub use dl_core::FieldType;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataChunk {
    pub rows: Vec<DataRow>,
}

/// One v2 row: the values of the selected fields, and the legend ids they belong to.
///
/// Python sends `{"data": [...], "legend": [...]}` (`make_data_response_v2`) precisely so a
/// legend can hold items (`info`, `template`, `parameter`) that have no value in the row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataRow {
    pub data: Vec<serde_json::Value>,
    pub legend: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockInfo {
    pub block_id: i64,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportPermission {
    pub allowed: bool,
    pub reason: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataExport {
    pub background: ExportPermission,
    pub basic: ExportPermission,
}

impl DataExport {
    /// `DataExportForbiddenReason.disabled_in_conn`.
    pub const DISABLED_IN_CONNECTION: &'static str = "DISABLED_EXPORT_CONNECTION";
    /// `DataExportForbiddenReason.disabled_in_ds`.
    pub const DISABLED_IN_DATASET: &'static str = "DISABLED_EXPORT_DATASET";
    /// `DataExportForbiddenReason.disabled_in_tenant`.
    pub const DISABLED_IN_TENANT: &'static str = "DISABLED_EXPORT_TENANT";
    /// `DataExportForbiddenReason.prohibited_in_conn_type`.
    pub const PROHIBITED_IN_CONNECTION: &'static str = "PROHIBITED_EXPORT_CONNECTION";
    /// `DataExportForbiddenReason.prohibited_in_tenant`, i.e. `background_allowed_in_tenant`.
    pub const PROHIBITED_IN_TENANT: &'static str = "PROHIBITED_EXPORT_TENANT";

    pub fn allowed() -> Self {
        let allowed = ExportPermission {
            allowed: true,
            reason: None,
        };
        Self {
            background: allowed.clone(),
            basic: allowed,
        }
    }

    /// `enabled_in_ds == false`: `get_data_export_base_result` forbids both kinds of export
    /// and reports `DataExportForbiddenReason.disabled_in_ds`.
    pub fn forbidden(reason: &str) -> Self {
        let denied = ExportPermission {
            allowed: false,
            reason: Some(vec![reason.to_string()]),
        };
        Self {
            background: denied.clone(),
            basic: denied,
        }
    }

    /// `tenant.is_background_data_export_allowed`.
    ///
    /// The stock demo tenant cannot export in the background while a plain (file) export works,
    /// which is what every reference response in the corpus reports. A deployment that enables
    /// the tenant flag does not need a rebuilt binary: `DL_DATA_EXPORT_BACKGROUND_ALLOWED=1`.
    pub fn background_export_allowed() -> bool {
        std::env::var("DL_DATA_EXPORT_BACKGROUND_ALLOWED")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }

    /// `tenant.is_data_export_enabled`.
    ///
    /// The stock demo tenant allows a plain (file) export; a deployment whose tenant forbids it
    /// says so with `DL_DATA_EXPORT_TENANT_FORBIDDEN=1` instead of a rebuilt binary.
    pub fn tenant_export_allowed() -> bool {
        !std::env::var("DL_DATA_EXPORT_TENANT_FORBIDDEN")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    }

    /// The `data_export` block of one dataset: `get_data_export_base_result`.
    ///
    /// The five `DataExportInfo` inputs come from the connection (`enabled_in_conn`,
    /// `allowed_in_conn_type`), the dataset (`enabled_in_ds`) and the tenant
    /// (`enabled_in_tenant`, `background_allowed_in_tenant`). Without a resolved connection the
    /// connection inputs stay permissive, which is what a dry run and an inline preview have.
    pub fn for_dataset(dataset: &Dataset) -> Self {
        Self::for_dataset_in_connection(dataset, None)
    }

    /// [`DataExport::for_dataset`] with the connection policy the resolver returned.
    pub fn for_dataset_in_connection(
        dataset: &Dataset,
        connection: Option<&crate::executor::ConnectionTarget>,
    ) -> Self {
        let enabled_in_conn = connection
            .map(|connection| !connection.data_export_forbidden)
            .unwrap_or(true);
        let enabled_in_ds = !dataset.data_export_forbidden;
        let allowed_in_conn_type = connection
            .map(|connection| connection.data_export_allowed_in_conn_type)
            .unwrap_or(true);
        Self::from_parts(enabled_in_conn, enabled_in_ds, allowed_in_conn_type)
    }

    /// The `data_export` block of a connection-scoped request (`dashsql`, `typed_query`): there
    /// is no dataset, so `enabled_in_ds` is `true` (`DashSQLView.get_data_export_info`).
    pub fn for_connection(connection: &crate::executor::ConnectionTarget) -> Self {
        Self::from_parts(
            !connection.data_export_forbidden,
            true,
            connection.data_export_allowed_in_conn_type,
        )
    }

    fn from_parts(enabled_in_conn: bool, enabled_in_ds: bool, allowed_in_conn_type: bool) -> Self {
        let enabled_in_tenant = Self::tenant_export_allowed();
        let background_allowed_in_tenant = Self::background_export_allowed();

        let mut basic_reasons: Vec<String> = Vec::new();
        let mut reasons: Vec<String> = Vec::new();
        for (enabled, reason) in [
            (enabled_in_conn, Self::DISABLED_IN_CONNECTION),
            (enabled_in_ds, Self::DISABLED_IN_DATASET),
            (enabled_in_tenant, Self::DISABLED_IN_TENANT),
        ] {
            if !enabled {
                basic_reasons.push(reason.to_string());
                reasons.push(reason.to_string());
            }
        }
        if !allowed_in_conn_type {
            reasons.push(Self::PROHIBITED_IN_CONNECTION.to_string());
        }
        if !background_allowed_in_tenant {
            reasons.push(Self::PROHIBITED_IN_TENANT.to_string());
        }

        let permission = |allowed: bool, reasons: &[String]| ExportPermission {
            allowed,
            reason: (!reasons.is_empty()).then(|| reasons.to_vec()),
        };
        Self {
            basic: permission(basic_reasons.is_empty(), &basic_reasons),
            background: permission(reasons.is_empty(), &reasons),
        }
    }
}

/// `DataApiV2ResponseSchema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultResponse {
    pub fields: Vec<LegendItem>,
    pub result_data: Vec<DataChunk>,
    pub blocks: Vec<BlockInfo>,
    pub notifications: Vec<serde_json::Value>,
    pub data_export: DataExport,
    pub data_export_forbidden: bool,
    /// The totals block's row, for the tests and for the v1 responses of M10.
    ///
    /// `DataApiV2ResponseSchema` has no place for it, so it is not on the wire: Python's
    /// `make_data_response_v2` builds `result_data` from the merged stream, which already holds
    /// the row (see [`crate::service::Service::result`]).
    #[serde(skip)]
    pub totals: Option<DataRow>,
    /// The SQL of the totals block, for the v1 envelope's `result.totals_query`.
    #[serde(skip)]
    pub totals_query: Option<String>,
    /// Exact compact-JSON length of this body, measured once when the service finalizes it.
    #[serde(skip)]
    pub json_len: u64,
}

impl ResultResponse {
    /// Measure the compact-JSON length once, after the body is final; the engine reports it as
    /// `sources[].size` and never serializes the body to learn it.
    pub fn measure_json_len(&mut self) {
        self.json_len = result_json_len(self);
    }
}

impl PartialEq for ResultResponse {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
            && self.result_data == other.result_data
            && self.blocks == other.blocks
            && self.notifications == other.notifications
            && self.data_export == other.data_export
            && self.data_export_forbidden == other.data_export_forbidden
            && self.totals == other.totals
            && self.totals_query == other.totals_query
    }
}

impl ResultResponse {
    /// The first row set, if the query returned anything.
    pub fn rows(&self) -> Option<&Vec<DataRow>> {
        self.result_data.first().map(|chunk| &chunk.rows)
    }

    /// The values of the first row set, without the legend ids.
    pub fn values(&self) -> Option<Vec<&Vec<serde_json::Value>>> {
        self.rows()
            .map(|rows| rows.iter().map(|row| &row.data).collect())
    }

    /// `DataRequestResponseSerializer.make_data_response_v1`: the legacy `v1` envelope.
    ///
    /// The dataset editor reads it — `sdk.bi.getPreview` is
    /// `POST /api/data/v1/datasets/{ds_id}/versions/{version}/preview`
    /// (`shared/schema/bi/actions/datasets.js`). Two shapes differ from [`ResultResponse`]'s:
    /// the rows are one `Data` array under `result.data`, and `result.data.Type` describes the
    /// columns with a YQL schema (`UserDataType::yql_name`) instead of the `v2` legend. Python
    /// also writes `result.query` (only for the `result`/`pivot` query types,
    /// `_SHOW_QUERY_FOR_QUERY_TYPES`), `result.totals`/`totals_query` and `result.fields` (only
    /// with `add_fields_data`); a preview sets none of them — it is not a `result` query, asks
    /// for no totals and the preview schema leaves `add_fields_data` at `false` — so the
    /// envelope here holds neither.
    pub fn to_v1(&self) -> serde_json::Value {
        let columns: Vec<serde_json::Value> = self
            .fields
            .iter()
            .map(|item| {
                serde_json::json!([
                    item.title,
                    ["OptionalType", ["DataType", item.data_type.yql_name()]]
                ])
            })
            .collect();
        let rows: Vec<&Vec<serde_json::Value>> = self
            .result_data
            .iter()
            .flat_map(|chunk| chunk.rows.iter().map(|row| &row.data))
            .collect();
        serde_json::json!({
            "result": {
                "data": {
                    "Data": rows,
                    "Type": ["ListType", ["StructType", columns]],
                },
                "data_export_forbidden": self.data_export_forbidden,
            },
            "data_export": self.data_export,
        })
    }
}

impl ResultResponse {
    /// `DatasetVersionResultResponseSchema`: the same legacy envelope [`Self::to_v1`] builds,
    /// plus the three keys a `/result` request sets and a preview does not.
    ///
    /// Python's `make_data_response_v1` writes `result.query` for the `result`/`pivot` query
    /// types, `result.totals`/`totals_query` when the request asked for totals, and
    /// `result.fields` when `add_fields_data` is on (the v1/v1.5 default).
    pub fn to_v1_result(&self, dataset: &Dataset, add_fields_data: bool) -> serde_json::Value {
        let mut body = self.to_v1();
        let Some(result) = body
            .get_mut("result")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return body;
        };
        if let Some(query) = self
            .blocks
            .first()
            .and_then(|block| block.query.as_deref())
            .filter(|query| !query.is_empty())
        {
            result.insert("query".to_string(), serde_json::json!(query));
        }
        if let Some(totals) = &self.totals {
            result.insert("totals".to_string(), serde_json::json!(totals.data));
        }
        if self.totals.is_some() || self.totals_query.is_some() {
            // `make_data_response_v1` writes the pair whenever either side is known, so a
            // filtered-out totals row is `"totals": null` next to its query, not a missing key.
            if self.totals.is_none() {
                result.insert("totals".to_string(), serde_json::Value::Null);
            }
            result.insert(
                "totals_query".to_string(),
                serde_json::json!(self.totals_query),
            );
        }
        if add_fields_data {
            let fields: Vec<serde_json::Value> = dataset
                .fields
                .iter()
                .map(|field| {
                    let data_type = field
                        .data_type
                        .or(field.cast)
                        .or(field.initial_data_type)
                        .unwrap_or(UserDataType::String);
                    serde_json::json!({
                        "title": field.title,
                        "guid": field.guid,
                        "data_type": data_type,
                        "calc_mode": field.calc_mode,
                    })
                })
                .collect();
            result.insert("fields".to_string(), serde_json::Value::Array(fields));
        }
        body
    }

    /// `DatasetRangeViewV1._transpose_range_data`: v1/v1.5 `/values/range` answer two rows of
    /// one column, where v2 answers one row of two columns.
    pub fn to_v1_range(&self) -> serde_json::Value {
        let mut body = self.to_v1();
        let Some(result) = body
            .get_mut("result")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return body;
        };
        let Some(row) = self.rows().and_then(|rows| rows.first()) else {
            return body;
        };
        let data = match (row.data.first(), row.data.get(1)) {
            (Some(min), Some(max)) => serde_json::json!([[min], [max]]),
            _ => serde_json::json!([]),
        };
        if let Some(data_object) = result
            .get_mut("data")
            .and_then(serde_json::Value::as_object_mut)
        {
            data_object.insert("Data".to_string(), data);
            // `Type` describes the one remaining column: keep only the first struct field.
            if let Some(columns) = data_object
                .get_mut("Type")
                .and_then(|value| value.as_array_mut())
                .and_then(|list| list.get_mut(1))
                .and_then(|value| value.as_array_mut())
                .and_then(|list| list.get_mut(1))
                .and_then(|value| value.as_array_mut())
            {
                columns.truncate(1);
            }
        }
        body
    }
}

// ---------------------------------------------------------------------------------------
// Pivot: `dl_api_lib.schemas.pivot` (wire) + `dl_pivot.primitives` (values)
// ---------------------------------------------------------------------------------------

/// `DataAPILimits.PIVOT_API_DEFAULT_ROW_COUNT_HARD_LIMIT`.
///
/// `/result` allows a million rows, `/pivot` only a hundred thousand: every row becomes one
/// cell of the table, and the whole table is built in memory.
pub const PIVOT_DEFAULT_ROW_LIMIT: i64 = 100_000;

/// `POST /api/data/v2/datasets/{ds_id}/pivot` (`PivotDataRequestBaseSchema`).
///
/// The body is a `/result` body without `result`, plus `pivot`: the same legend (`fields`,
/// `order_by`, `filters`, `updates`, ...) describes the single query that feeds the table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotRequest {
    #[serde(default)]
    pub fields: Vec<FieldsItem>,
    #[serde(default)]
    pub order_by: Vec<OrderByItem>,
    #[serde(default)]
    pub filters: Vec<FilterItem>,
    /// `RequestPivotSpecSchema`; an absent object is an empty structure (`RawPivotSpec()`).
    #[serde(default)]
    pub pivot: PivotSpec,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub offset: Option<i64>,
    #[serde(default)]
    pub disable_group_by: bool,
    /// `row_count_hard_limit`: the pivot default is [`PIVOT_DEFAULT_ROW_LIMIT`].
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub row_count_hard_limit: Option<i64>,
    #[serde(default)]
    pub updates: Vec<UpdateAction>,
    #[serde(default)]
    pub autofill_legend: bool,
    #[serde(default)]
    pub parameter_values: Vec<ParameterValueItem>,
    #[serde(default)]
    pub ignore_nonexistent_filters: bool,
    /// The pinned dataset revision (see [`ResultRequest::revision_id`]); the charts engine
    /// sends `dataset_revision_id`, so both names are accepted.
    #[serde(default, alias = "dataset_revision_id")]
    pub revision_id: Option<String>,
    /// Inline dataset, as `/datasets/data/preview` sends it.
    #[serde(default)]
    pub dataset: Option<Dataset>,
}

impl PivotRequest {
    /// `row_count_hard_limit`, defaulted per endpoint (`PivotDataRequestBaseSchema`).
    pub fn hard_limit(&self) -> i64 {
        self.row_count_hard_limit
            .unwrap_or(PIVOT_DEFAULT_ROW_LIMIT)
            .max(0)
    }
}

/// `RequestPivotSpecSchema` (`RawPivotSpec`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotSpec {
    #[serde(default)]
    pub structure: Vec<PivotSpecItem>,
    #[serde(default)]
    pub pagination: PivotPagination,
    /// `with_totals: null` means "derive from `totals`"; the port rejects both anyway.
    #[serde(default)]
    pub with_totals: Option<bool>,
    #[serde(default)]
    pub totals: Option<PivotTotals>,
}

impl PivotSpec {
    /// Whether the request asks for totals, i.e. for the multi-block path.
    ///
    /// `PivotSpecNormalizer` expands `with_totals` into `totals` itself, so either flag means
    /// the same thing: extra blocks (`FieldRole.template` dimensions, `FieldRole.total`
    /// measures) merged into one stream. An explicitly empty `totals` (`{"rows": [],
    /// "columns": []}`) is what the charts wizard sends when the chart has totals switched
    /// off, and it must not turn the request into a totals one.
    pub fn wants_totals(&self) -> bool {
        if self.with_totals == Some(true) {
            return true;
        }
        self.totals
            .as_ref()
            .is_some_and(|totals| !totals.rows.is_empty() || !totals.columns.is_empty())
    }
}

/// `RequestPivotItemSchema` (`RawPivotLegendItem`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotSpecItem {
    /// Accepted and ignored: `make_pivot_legend` hands out its own ids from zero.
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub pivot_item_id: Option<i64>,
    #[serde(default)]
    pub legend_item_ids: Vec<i64>,
    pub role_spec: PivotRoleSpec,
    #[serde(default)]
    pub title: Option<String>,
}

/// `PivotPaginationSchema` (`PivotPaginationSpec`): crops the *finished* table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotPagination {
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub limit_rows: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub offset_rows: Option<i64>,
}

impl PivotPagination {
    /// `PivotPaginationSpec.non_default`: without it `paginate()` is not called at all.
    pub fn non_default(&self) -> bool {
        self.limit_rows.is_some() || self.offset_rows.is_some()
    }
}

/// `PivotTotalsSchema` (`RawPivotTotalsSpec`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotTotals {
    #[serde(default)]
    pub rows: Vec<PivotTotalsItem>,
    #[serde(default)]
    pub columns: Vec<PivotTotalsItem>,
}

/// `PivotTotalsItemSchema`: `level` is the number of dimensions left in place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotTotalsItem {
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub level: Option<i64>,
}

/// `dl_constants.enums.PivotRole`: `pivot_row`, `pivot_column`, `pivot_measure`,
/// `pivot_annotation`, `pivot_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PivotRole {
    #[default]
    #[serde(rename = "pivot_row")]
    Row,
    #[serde(rename = "pivot_column")]
    Column,
    #[serde(rename = "pivot_measure")]
    Measure,
    #[serde(rename = "pivot_annotation")]
    Annotation,
    #[serde(rename = "pivot_info")]
    Info,
}

impl PivotRole {
    /// `PivotRole.name`, as the API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Row => "pivot_row",
            Self::Column => "pivot_column",
            Self::Measure => "pivot_measure",
            Self::Annotation => "pivot_annotation",
            Self::Info => "pivot_info",
        }
    }
}

/// `PivotRoleSpecSchema` (`OneOfSchema` on `role`), flattened into one optional-fields struct.
///
/// The variants are `DimensionPivotRoleSpec` (`direction`), `MeasurePivotRoleSpec`
/// (`sorting`), `AnnotationPivotRoleSpec` (`annotation_type`, `target_legend_item_ids`) and
/// the plain one (`role` only). Marshmallow dumps the variant's own fields, `null` included —
/// a measure always carries `"sorting": null`, an annotation always carries
/// `"target_legend_item_ids"` — so [`Serialize`] is written per variant instead of skipping
/// the empty options.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PivotRoleSpec {
    pub role: PivotRole,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub sorting: Option<PivotMeasureSorting>,
    #[serde(default)]
    pub annotation_type: Option<String>,
    #[serde(default)]
    pub target_legend_item_ids: Option<Vec<i64>>,
}

impl Serialize for PivotRoleSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("role", &self.role)?;
        match self.role {
            PivotRole::Row | PivotRole::Column => {
                map.serialize_entry("direction", &self.direction.unwrap_or(Direction::Asc))?
            }
            // `MeasurePivotRoleSpecSchemaVariant.sorting`: `allow_none=True`.
            PivotRole::Measure => map.serialize_entry("sorting", &self.sorting)?,
            PivotRole::Annotation => {
                map.serialize_entry("annotation_type", &self.annotation_type)?;
                map.serialize_entry("target_legend_item_ids", &self.target_legend_item_ids)?;
            }
            // `PivotRoleSpecSchemaVariant`: the role and nothing else.
            PivotRole::Info => {}
        }
        map.end()
    }
}

impl PivotRoleSpec {
    pub fn new(role: PivotRole) -> Self {
        Self {
            role,
            ..Self::default()
        }
    }

    /// `PivotDimensionRoleSpec.direction`: `asc` unless the client says otherwise.
    pub fn dimension_direction(&self) -> Direction {
        self.direction.unwrap_or_default()
    }
}

/// `PivotMeasureSortingSchema` (`PivotMeasureSorting`): the measure sorts one axis.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotMeasureSorting {
    #[serde(default)]
    pub column: Option<PivotMeasureSortingSettings>,
    #[serde(default)]
    pub row: Option<PivotMeasureSortingSettings>,
}

/// `PivotMeasureSortingSettingsSchema`: which header to sort by, and in which direction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotMeasureSortingSettings {
    #[serde(default)]
    pub header_values: Vec<PivotHeaderValue>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub role_spec: PivotHeaderRoleSpec,
}

/// `PivotHeaderValueSchema.value`: the schema insists on a string, even for numeric
/// dimensions (`PivotHeader.compare_sorting_settings` then compares it with a raw value).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotHeaderValue {
    #[serde(default)]
    pub value: String,
}

/// `dl_constants.enums.PivotHeaderRole`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PivotHeaderRole {
    #[default]
    Data,
    Total,
}

/// `PivotHeaderRoleSpecSchema`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotHeaderRoleSpec {
    #[serde(default)]
    pub role: PivotHeaderRole,
}

/// `PivotHeaderInfoSchema`: a column/row header plus how it is sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PivotHeaderInfo {
    #[serde(default)]
    pub sorting_direction: Option<Direction>,
    #[serde(default)]
    pub role_spec: PivotHeaderRoleSpec,
}

/// One entry of the response `pivot.structure` (`PivotItemSchema`).
///
/// `item_type` is *not* part of the schema: `make_pivot_response` dumps the pivot legend
/// through `PivotItemSchema`, which drops it, so it is kept out of this struct as well.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PivotItem {
    pub pivot_item_id: i64,
    pub legend_item_ids: Vec<i64>,
    pub role_spec: PivotRoleSpec,
    pub title: String,
}

/// The response `pivot` object (`TODO: Dump pivot_table.pivot_legend` in Python: only the
/// structure is sent, with the patched `Measure Names`/`Dimension Names` items included).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotStructure {
    #[serde(default)]
    pub structure: Vec<PivotItem>,
}

/// One pivot cell of the response: `[value, legend_item_id, pivot_item_id]`.
///
/// Python builds the triple in `PivotDataRequestResponseSerializer._serialize_cell_vector`;
/// the client abstraction (`data_abstraction/pivot.py`) reads only `[0]` and `[1]`.
pub type PivotCell = (serde_json::Value, i64, i64);

/// A serialized `DataCellVector`: `null` for a hole in the table.
pub type PivotCellVector = Option<Vec<PivotCell>>;

/// `{cells, header_info}` (`columns_with_info` and `rows[].header_with_info`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotHeaderWithInfo {
    #[serde(default)]
    pub cells: Vec<PivotCellVector>,
    #[serde(default)]
    pub header_info: PivotHeaderInfo,
}

/// `{header, header_with_info, values}`: one row of the table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotRow {
    #[serde(default)]
    pub header: Vec<PivotCellVector>,
    #[serde(default)]
    pub header_with_info: PivotHeaderWithInfo,
    #[serde(default)]
    pub values: Vec<PivotCellVector>,
}

/// The response `pivot_data` object (`make_pivot_response`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PivotData {
    /// Columns without their headers; `columns_with_info` repeats the cells plus `info`.
    #[serde(default)]
    pub columns: Vec<Vec<PivotCellVector>>,
    #[serde(default)]
    pub columns_with_info: Vec<PivotHeaderWithInfo>,
    /// One cell per `pivot_row` item: the titles of the row dimensions.
    #[serde(default)]
    pub row_dimension_headers: Vec<PivotCellVector>,
    #[serde(default)]
    pub rows: Vec<PivotRow>,
}

/// `PivotDataRequestResponseSerializer.make_pivot_response`: the `/pivot` answer.
///
/// `fields` is the merged-stream legend (`LegendSchema`), which is why it also carries the
/// `Measure Names`/`Dimension Names` pseudo-items the legend formalizer adds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PivotResponse {
    pub fields: Vec<LegendItem>,
    pub pivot: PivotStructure,
    pub pivot_data: PivotData,
    pub blocks: Vec<BlockInfo>,
    pub notifications: Vec<serde_json::Value>,
    pub data_export: DataExport,
    /// Absent from the body: `PivotDataRequestResponseSerializer.make_pivot_response` never sets
    /// `data_export_forbidden` (only `make_data_response_v2` does), and the pivot schema has no
    /// attribute to dump, so the key is missing from every reference pivot response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_export_forbidden: Option<bool>,
    /// Exact compact-JSON length of this body (see [`ResultResponse::json_len`]).
    #[serde(skip)]
    pub json_len: u64,
}

impl PivotResponse {
    /// See [`ResultResponse::measure_json_len`].
    pub fn measure_json_len(&mut self) {
        self.json_len = json_len(self);
    }
}

impl PartialEq for PivotResponse {
    fn eq(&self, other: &Self) -> bool {
        self.fields == other.fields
            && self.pivot == other.pivot
            && self.pivot_data == other.pivot_data
            && self.blocks == other.blocks
            && self.notifications == other.notifications
            && self.data_export == other.data_export
            && self.data_export_forbidden == other.data_export_forbidden
    }
}

/// `DatasetVersionValuesDistinctPostSchema` (v1) and `DistinctDataRequestV2Schema` (v2).
///
/// The two bodies name the field differently: v1 sends `field_guid`, v2 sends the same
/// `{ref, role_spec}` list `/result` takes (`NewDatasetDataRequestBaseSchema.fields`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DistinctRequest {
    /// The v1 key; the v2 body leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_guid: Option<String>,
    /// The v2 key: exactly one item, with the `distinct` role.
    #[serde(default)]
    pub fields: Vec<FieldsItem>,
    /// The request key is `where` in v1 and `filters` in v2: the same list.
    #[serde(rename = "where", alias = "filters", default)]
    pub filters: Vec<FilterItem>,
    #[serde(default)]
    pub updates: Vec<UpdateAction>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub offset: Option<i64>,
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub row_count_hard_limit: Option<i64>,
    #[serde(default)]
    pub ignore_nonexistent_filters: bool,
    #[serde(default)]
    pub parameter_values: Vec<ParameterValueItem>,
    /// Pins the query-cache key to the dataset revision the engine got from `/fields`; an
    /// external client that omits it simply gets an uncached answer, like `/result`.
    #[serde(
        default,
        alias = "dataset_revision_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub revision_id: Option<String>,
}

impl DistinctRequest {
    /// The field to take the distinct values of: `fields` (v2) wins over `field_guid` (v1).
    pub fn field_ref(&self) -> Option<FieldRef> {
        selected_field_ref(&self.fields, self.field_guid.as_deref())
    }
}

/// `DatasetVersionValuesRangePostSchema` / `RangeDataRequestV2Schema`; same two body shapes as
/// [`DistinctRequest`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RangeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_guid: Option<String>,
    #[serde(default)]
    pub fields: Vec<FieldsItem>,
    #[serde(rename = "where", alias = "filters", default)]
    pub filters: Vec<FilterItem>,
    #[serde(default)]
    pub updates: Vec<UpdateAction>,
    #[serde(default)]
    pub ignore_nonexistent_filters: bool,
    #[serde(default)]
    pub parameter_values: Vec<ParameterValueItem>,
}

impl RangeRequest {
    /// The field to take the range of: `fields` (v2) wins over `field_guid` (v1).
    pub fn field_ref(&self) -> Option<FieldRef> {
        selected_field_ref(&self.fields, self.field_guid.as_deref())
    }
}

/// The single field a `/values/*` request is about, whichever body shape carried it.
fn selected_field_ref(fields: &[FieldsItem], field_guid: Option<&str>) -> Option<FieldRef> {
    fields
        .first()
        .map(|item| item.field_ref.clone())
        .or_else(|| field_guid.map(FieldRef::id))
}

/// `POST /api/data/v2/datasets/{ds_id}/result-preflight`.
///
/// It takes the same body a `/result` call takes (`ResultDataRequestV2Schema`); the response
/// is the dataset as the validator sees it (`ResultPreflightResponseSchema`):
/// `{"code": "OK", "message": "...", "dataset": {"component_errors": {"items": []}, "fields": []}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreflightResponse {
    pub code: String,
    pub message: String,
    pub dataset: PreflightDataset,
}

/// `ResultPreflightDatasetResponseSchema`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PreflightDataset {
    /// `ComponentErrorListSchema`: `{"items": [...]}`, empty when the dataset is valid.
    #[serde(default)]
    pub component_errors: dl_core::ComponentErrorRegistry,
    /// `get_fields_data_raw`, narrowed by `only=("title", "guid", "data_type", "calc_mode",
    /// "type")`; omitted for a fatal error, which is what `fields: None` serialises to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<PreflightField>>,
}

/// `FieldsResponseFieldSchema` with the five keys the preflight schema keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightField {
    pub title: String,
    pub guid: String,
    /// `null` for a field whose type cannot be read at all, exactly like `fld.data_type`.
    pub data_type: Option<UserDataType>,
    pub calc_mode: dl_core::CalcMode,
    #[serde(rename = "type")]
    pub kind: FieldType,
}

/// `POST /api/data/v1/connections/{conn_id}/dashsql` (`DashSQLRequestSchema`).
///
/// Python names the SQL `sql_query`; older clients and the task description call it `sql`, so
/// both spellings are accepted. `params`/`db_params`/`connector_specific_params` are read but
/// not yet substituted into the SQL (the port has no Jinja/DBAPI formatter).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DashSqlRequest {
    #[serde(default, alias = "sql")]
    pub sql_query: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub db_params: Option<serde_json::Value>,
    #[serde(default)]
    pub connector_specific_params: Option<serde_json::Value>,
    /// `dl_connector_clickhouse` connection setting; accepted for compatibility.
    #[serde(default, deserialize_with = "lenient_int::optional")]
    pub max_execution_time: Option<i64>,
}

/// `POST /api/data/v1/connections/{conn_id}/typed_query` (`TypedQuerySchema`).
///
/// The documented shape is `{query_type, query_content: {query}, parameters}`; a body that
/// carries the SQL as a bare `query` string is accepted too.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypedQueryRequest {
    #[serde(default)]
    pub query_type: Option<String>,
    #[serde(default)]
    pub query_content: Option<TypedQueryContent>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub parameters: Vec<serde_json::Value>,
}

/// `PlainTypedQueryContentSchema`: the only content shape the plain loader knows.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypedQueryContent {
    pub query: String,
}

impl TypedQueryRequest {
    /// The SQL of the typed query, whichever key carried it.
    pub fn sql(&self) -> Option<&str> {
        self.query_content
            .as_ref()
            .map(|content| content.query.as_str())
            .or(self.query.as_deref())
            .filter(|query| !query.trim().is_empty())
    }

    /// `query_type.name`; the plain loader's type is `generic_query`.
    pub fn query_type(&self) -> &str {
        self.query_type.as_deref().unwrap_or("generic_query")
    }
}

/// `POST /api/data/v1/connections/{conn_id}/typed_query_raw` (`TypedQueryRawSchema`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypedQueryRawRequest {
    #[serde(default)]
    pub query_type: Option<String>,
    #[serde(default)]
    pub parameters: Option<TypedQueryRawParameters>,
}

/// `TypedQueryRawParametersSchema`: a raw HTTP request to the connector's own API.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypedQueryRawParameters {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub method: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `/values/range` echoes the `range_type` of both of its items back to the client.
    #[test]
    fn a_range_role_spec_keeps_its_range_type() {
        let raw = json!({"role": "range", "range_type": "max"});
        let spec: RoleSpec = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(spec.range_type, Some(RangeType::Max));
        assert_eq!(serde_json::to_value(&spec).unwrap(), raw);
    }

    /// `PivotRoleSpecSchema` variants: marshmallow dumps the variant's own fields, `null`
    /// included, so a measure always carries `sorting` and an annotation its target ids.
    #[test]
    fn a_pivot_role_spec_is_serialised_per_variant() {
        let spec = PivotRoleSpec {
            direction: Some(Direction::Desc),
            ..PivotRoleSpec::new(PivotRole::Column)
        };
        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            json!({"role": "pivot_column", "direction": "desc"})
        );

        // `sorting` is `allow_none=True` in `MeasurePivotRoleSpecSchemaVariant`.
        assert_eq!(
            serde_json::to_value(PivotRoleSpec::new(PivotRole::Measure)).unwrap(),
            json!({"role": "pivot_measure", "sorting": null})
        );

        let annotation = PivotRoleSpec {
            annotation_type: Some("color".to_string()),
            target_legend_item_ids: Some(vec![2]),
            ..PivotRoleSpec::new(PivotRole::Annotation)
        };
        assert_eq!(
            serde_json::to_value(&annotation).unwrap(),
            json!({
                "role": "pivot_annotation",
                "annotation_type": "color",
                "target_legend_item_ids": [2],
            })
        );

        // `pivot_info` has the plain variant: the role and nothing else.
        assert_eq!(
            serde_json::to_value(PivotRoleSpec::new(PivotRole::Info)).unwrap(),
            json!({"role": "pivot_info"})
        );
    }

    /// `getPreview` answers the legacy envelope: the rows move under `result.data.Data` and the
    /// columns become a YQL schema (`bi_to_yql`), while `query` (`result`/`pivot` only) and
    /// `fields` (`add_fields_data`) stay out of it.
    #[test]
    fn the_v1_preview_envelope_wraps_the_rows_in_the_yql_schema() {
        let response = ResultResponse {
            fields: vec![
                LegendItem {
                    legend_item_id: 0,
                    id: "f_region".to_string(),
                    title: "Region".to_string(),
                    data_type: UserDataType::String,
                    field_type: FieldType::Dimension,
                    item_type: "field".to_string(),
                    role_spec: RoleSpec::new(Role::Row),
                },
                LegendItem {
                    legend_item_id: 1,
                    id: "f_sales".to_string(),
                    title: "Sales".to_string(),
                    data_type: UserDataType::Float,
                    field_type: FieldType::Measure,
                    item_type: "field".to_string(),
                    role_spec: RoleSpec::new(Role::Measure),
                },
            ],
            result_data: vec![DataChunk {
                rows: vec![DataRow {
                    data: vec![json!("West"), json!(10.5)],
                    legend: vec![0, 1],
                }],
            }],
            blocks: vec![BlockInfo {
                block_id: 0,
                query: Some("SELECT 1".to_string()),
            }],
            notifications: Vec::new(),
            data_export: DataExport::allowed(),
            data_export_forbidden: false,
            totals: None,
            totals_query: None,
            json_len: 0,
        };
        let v1 = response.to_v1();
        assert_eq!(v1["result"]["data"]["Data"], json!([["West", 10.5]]));
        assert_eq!(
            v1["result"]["data"]["Type"],
            json!([
                "ListType",
                [
                    "StructType",
                    [
                        ["Region", ["OptionalType", ["DataType", "String"]]],
                        ["Sales", ["OptionalType", ["DataType", "Double"]]],
                    ]
                ]
            ])
        );
        assert_eq!(v1["result"]["data_export_forbidden"], json!(false));
        assert!(v1["result"].get("query").is_none());
        assert!(v1["result"].get("fields").is_none());
        assert_eq!(v1["data_export"]["basic"]["allowed"], json!(true));
    }

    /// `make_data_response_v1`: once a totals query exists, the pair is written even when the
    /// totals row itself is `None` (everything was filtered out) — the key is `null`, not absent.
    #[test]
    fn the_v1_result_reports_a_filtered_out_totals_row_as_null() {
        let response = ResultResponse {
            fields: Vec::new(),
            result_data: Vec::new(),
            blocks: Vec::new(),
            notifications: Vec::new(),
            data_export: DataExport::allowed(),
            data_export_forbidden: false,
            totals: None,
            totals_query: Some("SELECT sum(x) FROM t".to_string()),
            json_len: 0,
        };
        let v1 = response.to_v1_result(&Dataset::default(), false);
        assert!(v1["result"].as_object().unwrap().contains_key("totals"));
        assert_eq!(v1["result"]["totals"], serde_json::Value::Null);
        assert_eq!(v1["result"]["totals_query"], json!("SELECT sum(x) FROM t"));
    }

    /// The wizard sends `"totals": {"rows": [], "columns": []}` when the chart has no
    /// totals at all, which must not switch the request to the multi-block path.
    #[test]
    fn empty_totals_are_not_totals() {
        let spec: PivotSpec = serde_json::from_value(json!({
            "structure": [],
            "pagination": {},
            "totals": {"rows": [], "columns": []}
        }))
        .expect("an empty totals object is valid");
        assert!(!spec.wants_totals());
    }

    #[test]
    fn a_totals_level_asks_for_totals() {
        let spec: PivotSpec = serde_json::from_value(json!({
            "structure": [],
            "totals": {"rows": [{"level": 0}], "columns": []}
        }))
        .expect("a totals level is valid");
        assert!(spec.wants_totals());

        let flag: PivotSpec =
            serde_json::from_value(json!({"structure": [], "with_totals": true})).unwrap();
        assert!(flag.wants_totals());
    }

    /// The editor's "Number of rows" input posts what it shows, so `limit` arrives as a
    /// *string* (`"100"`); pydantic coerces it and so must the port, or the preview is a 422.
    #[test]
    fn a_stringly_typed_limit_is_read_as_a_number() {
        let request: PreviewRequest =
            serde_json::from_value(json!({"limit": "100", "row_count_hard_limit": "1000"}))
                .expect("a numeric string is a valid integer");
        assert_eq!(request.limit, Some(100));
        assert_eq!(request.row_count_hard_limit, Some(1000));

        // An empty string is "no value" rather than an error, and a missing key is `None`.
        let empty: PreviewRequest = serde_json::from_value(json!({"limit": ""})).unwrap();
        assert_eq!(empty.limit, None);
        let absent: PreviewRequest = serde_json::from_value(json!({})).unwrap();
        assert_eq!(absent.limit, None);

        // Reasonable coercion of the other spellings pydantic accepts.
        let truncated: PreviewRequest = serde_json::from_value(json!({"limit": "100.9"})).unwrap();
        assert_eq!(truncated.limit, Some(100));
        let null: PreviewRequest = serde_json::from_value(json!({"limit": null})).unwrap();
        assert_eq!(null.limit, None);

        // The same rule covers the legend ids and every other request integer.
        let result: ResultRequest = serde_json::from_value(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "legend_item_id": "3",
                        "role_spec": {"role": "tree", "level": "2"}, "block_id": "1"}],
            "limit": "10",
            "offset": "20",
        }))
        .expect("legend ids may be strings too");
        assert_eq!(result.fields[0].legend_item_id, Some(3));
        assert_eq!(result.fields[0].block_id, Some(1));
        assert_eq!(result.fields[0].role_spec.as_ref().unwrap().level, Some(2));
        assert_eq!((result.limit, result.offset), (Some(10), Some(20)));

        // A value that is not a number is still an error: leniency is not silence.
        assert!(serde_json::from_value::<PreviewRequest>(json!({"limit": "many"})).is_err());

        // `dimension_values` is parsed separately; a dropped id would silently unlock the level.
        let tree: RoleSpec = serde_json::from_value(json!({
            "role": "tree",
            "level": "2",
            "prefix": "[]",
            "dimension_values": [{"legend_item_id": "1", "value": r#"["Electronics"]"#}],
        }))
        .unwrap();
        assert_eq!(tree.tree_dimension_values()[0].legend_item_id, 1);
    }
}
