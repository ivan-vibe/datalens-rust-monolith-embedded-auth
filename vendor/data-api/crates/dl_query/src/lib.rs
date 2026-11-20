//! Query compilation: a dataset plus a request spec, compiled to SQL.
//!
//! This is the Rust replacement for `dl_query_processing.compilation` +
//! `dl_query_processing.translation`. The formula part is delegated to `dfl_native`, which
//! already reproduces the `dl_formula` registry byte for byte; this crate owns the parts
//! that are *not* templated in `dl_formula`: `SELECT`/`FROM`/`WHERE`/`GROUP BY`/`HAVING`/
//! `ORDER BY`, aggregation wrapping, parameter substitution and the request spec ->
//! expression walk.

pub mod compiler;
pub mod fork;
pub mod identity;
pub mod literal;
pub mod plan;

use dl_core::{DataType, FieldRef};
use serde::{Deserialize, Serialize};

pub use compiler::{
    infer_field_data_types,
    check_formulas, compile, compile_distinct, compile_range, field_data_types, field_types,
    field_types_and_data_types, CompiledQuery, OutputColumn, QueryError,
};
pub use identity::{with_identity, Identity};
pub use literal::{Dialect, LiteralError};
pub use plan::{Join, Select, SelectExpr, Source};

/// `dl_constants.enums.FieldRole`: what the field is used for, as the API sends it.
///
/// The value a plain dimension is sent with is `row`, not `dimension`: `FieldRole` has no
/// `dimension` member (`datalens-ui` and `dl_api_client` both send `row`). `dimension` is
/// still accepted as an alias for the requests this port documented before M2b.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    #[serde(alias = "dimension")]
    Row,
    Measure,
    Info,
    /// `order_by` in the JSON; the role of one `order_by` item in the legend.
    #[serde(rename = "order_by")]
    OrderBy,
    Filter,
    Parameter,
    Distinct,
    Range,
    /// A totals row: it needs a second query, which the port does not build yet (M3).
    Total,
    Template,
    /// The `tree` role of `dl_query_processing`; not compiled yet.
    Tree,
}

impl Role {
    /// `dl_query_processing.legend.field_legend.SELECTABLE_ROLES`:
    /// these items are in the `SELECT` list, everything else lives in the legend only.
    pub fn is_selectable(self) -> bool {
        !matches!(
            self,
            Self::Info | Self::OrderBy | Self::Filter | Self::Parameter
        )
    }

    /// `FieldRole.name`, i.e. the string the API sends.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Measure => "measure",
            Self::Info => "info",
            Self::OrderBy => "order_by",
            Self::Filter => "filter",
            Self::Parameter => "parameter",
            Self::Distinct => "distinct",
            Self::Range => "range",
            Self::Total => "total",
            Self::Template => "template",
            Self::Tree => "tree",
        }
    }
}

/// One `SELECT` item (`QueryFieldsItemSchema`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectItem {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    #[serde(default)]
    pub role: Role,
    /// `TreeRoleSpec.level`: the `tree` role shows the first `level` entries of the array, so
    /// the expression is wrapped in `slice(expr, 1, level)`. `None` for every other role.
    #[serde(default)]
    pub level: Option<i64>,
}

impl SelectItem {
    pub fn new(field_ref: FieldRef, role: Role) -> Self {
        Self {
            field_ref,
            role,
            level: None,
        }
    }
}

/// Filtering directions as `dl_constants.enums.WhereClauseOperation` spells them.
///
/// `dl_api_lib` deserializes the marshmallow `EnumField` by *member name* (`IN`,
/// `ISNOTNULL`), while the port's own clients send the *value* (`in`), so
/// [`FilterOperation::parse`] accepts both spellings. Serialization keeps the value, which is
/// what `WhereClauseOperation.value` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterOperation {
    Isnull,
    Isnotnull,
    Gt,
    Lt,
    Gte,
    Lte,
    Eq,
    Ne,
    Startswith,
    Istartswith,
    Endswith,
    Iendswith,
    Contains,
    Icontains,
    Notcontains,
    Noticontains,
    In,
    Nin,
    Between,
    /// `LENEQ`/`LENNE`/`LENGT`/`LENGTE`/`LENLT`/`LENLTE`: compare `LEN(field)` with the value.
    /// `_generate_tree_filter_items` adds `LENGTE` to every `tree` item, so the tree only shows
    /// its first `level` entries; the rest come from the filter UI for array fields.
    Leneq,
    Lenne,
    Lengt,
    Lengte,
    Lenlt,
    Lenlte,
}

impl FilterOperation {
    /// `WhereClauseOperation`, by member name or by value, case-insensitively.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "isnull" => Self::Isnull,
            "isnotnull" => Self::Isnotnull,
            "gt" => Self::Gt,
            "lt" => Self::Lt,
            "gte" => Self::Gte,
            "lte" => Self::Lte,
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "startswith" => Self::Startswith,
            "istartswith" => Self::Istartswith,
            "endswith" => Self::Endswith,
            "iendswith" => Self::Iendswith,
            "contains" => Self::Contains,
            "icontains" => Self::Icontains,
            "notcontains" => Self::Notcontains,
            "noticontains" => Self::Noticontains,
            "in" => Self::In,
            "nin" | "notin" => Self::Nin,
            "between" => Self::Between,
            "leneq" => Self::Leneq,
            "lenne" => Self::Lenne,
            "lengt" => Self::Lengt,
            "lengte" => Self::Lengte,
            "lenlt" => Self::Lenlt,
            "lenlte" => Self::Lenlte,
            _ => return None,
        })
    }
}

impl<'de> Deserialize<'de> for FilterOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown filter operation `{raw}`, expected one of {}",
                "isnull, isnotnull, gt, lt, gte, lte, eq, ne, startswith, istartswith, \
                 endswith, iendswith, contains, icontains, notcontains, noticontains, in, nin, \
                 between, leneq, lenne, lengt, lengte, lenlt, lenlte (or the upper-case member \
                 names)"
            ))
        })
    }
}

/// One `where`/`filters` entry (`QueryFiltersItemSchema`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterSpec {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    pub operation: FilterOperation,
    #[serde(default)]
    pub values: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

/// One `order_by` entry (`QueryOrderByItemSchema`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBySpec {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    #[serde(default)]
    pub direction: Direction,
}

/// One `parameter_values` entry (`ParameterValueSchema` in `dl_api_lib.schemas.data`).
///
/// A parameter is a constant for one query: `{"ref": {...}, "value": <raw>}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterValueSpec {
    #[serde(rename = "ref")]
    pub field_ref: FieldRef,
    pub value: serde_json::Value,
}

/// Everything needed to build one `SELECT`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySpec {
    #[serde(default)]
    pub select: Vec<SelectItem>,
    #[serde(default)]
    pub filters: Vec<FilterSpec>,
    #[serde(default)]
    pub order_by: Vec<OrderBySpec>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// `GroupByPolicy.disable`: `/result` and `/pivot` take it from the request and refuse a
    /// measure selection with it (`InvalidGroupByConfiguration`).
    #[serde(default)]
    pub disable_group_by: bool,
    /// `GroupByPolicy.if_measures`: GROUP BY only when a selected item is a measure.
    ///
    /// That is the preview's policy: a dataset of dimensions is shown row by row, a dataset with
    /// measures is grouped like a chart would group it.
    #[serde(default)]
    pub group_by_if_measures: bool,
    /// Values of the dataset parameters, overriding their `default_value`.
    #[serde(default)]
    pub parameter_values: Vec<ParameterValueSpec>,
    /// `ignore_nonexistent_filters`: a filter over a missing field is dropped, not an error.
    #[serde(default)]
    pub ignore_nonexistent_filters: bool,
    /// `_make_totals`: the totals query carries no request blocks and no LOD/window forks. A
    /// measure whose formula needs one is `NULL` there — exactly what the reference emits
    /// (`SELECT sum(amount) AS res_0, NULL AS res_1`).
    #[serde(default)]
    pub totals: bool,
}

impl Default for QuerySpec {
    fn default() -> Self {
        Self {
            select: Vec::new(),
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            disable_group_by: false,
            group_by_if_measures: false,
            parameter_values: Vec::new(),
            ignore_nonexistent_filters: false,
            totals: false,
        }
    }
}

impl QuerySpec {
    /// The `row_count_hard_limit` guard `data-api` applies before executing anything.
    pub fn clamp_limit(&self, hard_limit: i64) -> Option<i64> {
        match self.limit {
            Some(limit) => Some(limit.min(hard_limit)),
            None => Some(hard_limit),
        }
    }
}

/// A field type as the legend reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegendField {
    pub guid: String,
    pub title: String,
    pub data_type: DataType,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WhereClauseOperation` reaches `data-api` as the marshmallow *member name* (`IN`),
    /// while the port's own requests spell the value (`in`); both must parse.
    #[test]
    fn filter_operations_parse_from_member_names_and_values() {
        let parse = |raw: &str| serde_json::from_str::<FilterOperation>(raw);
        assert_eq!(parse("\"IN\"").unwrap(), FilterOperation::In);
        assert_eq!(parse("\"in\"").unwrap(), FilterOperation::In);
        assert_eq!(parse("\"NIN\"").unwrap(), FilterOperation::Nin);
        assert_eq!(parse("\"ISNOTNULL\"").unwrap(), FilterOperation::Isnotnull);
        assert_eq!(parse("\"between\"").unwrap(), FilterOperation::Between);
        assert!(parse("\"like\"").is_err());
    }

    /// Serialization keeps the value, not the member name.
    #[test]
    fn filter_operations_serialize_as_their_values() {
        assert_eq!(
            serde_json::to_string(&FilterOperation::Noticontains).unwrap(),
            "\"noticontains\""
        );
        assert_eq!(
            serde_json::to_string(&FilterOperation::Nin).unwrap(),
            "\"nin\""
        );
    }
}
