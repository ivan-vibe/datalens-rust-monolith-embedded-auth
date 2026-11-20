//! The dataset object: fields, sources and the rules `DatasetValidator` enforces.
//!
//! Only the part the data query path needs is modelled. Everything is `serde`-friendly
//! because the object arrives from the request body (`/datasets/data/preview`) or from the
//! dataset store in exactly the shape DataLens serializes it in.

use dfl_native::{FieldName, FieldNames, FieldTypes};
use serde::{Deserialize, Serialize};

use crate::rls::{Rls, RlsEntry};

use crate::dtype::{DataType, UserDataType};
use crate::error::CoreError;

/// `dl_constants.enums.AggregationFunction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
    #[default]
    None,
    Sum,
    Avg,
    Min,
    Max,
    Count,
    Countunique,
}

impl Aggregation {
    /// Lenient parse of the `aggregation` string in the dataset JSON.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Some(Self::None),
            "sum" => Some(Self::Sum),
            "avg" => Some(Self::Avg),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            "count" => Some(Self::Count),
            "countunique" => Some(Self::Countunique),
            _ => None,
        }
    }

    pub fn is_aggregate(self) -> bool {
        self != Aggregation::None
    }

    /// The SQL function name, or `None` for a plain column.
    pub fn sql_name(self) -> Option<&'static str> {
        match self {
            Aggregation::None => None,
            Aggregation::Sum => Some("SUM"),
            Aggregation::Avg => Some("AVG"),
            Aggregation::Min => Some("MIN"),
            Aggregation::Max => Some("MAX"),
            Aggregation::Count => Some("COUNT"),
            Aggregation::Countunique => Some("COUNT_DISTINCT"),
        }
    }
}

/// `dl_constants.enums.CalcMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalcMode {
    #[default]
    Direct,
    Formula,
    /// A dataset parameter: a constant for the duration of one query, substituted from the
    /// request `parameter_values` or from `default_value`.
    Parameter,
}

/// `dl_constants.enums.FieldType`: how the dataset itself classifies a field.
///
/// The legend reports the type the *request* asked for, so this is the fallback used when a
/// request sends no role (and by `/fields`, which always describes the dataset as stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FieldType {
    Dimension,
    Measure,
}

impl FieldType {
    /// Lenient parse: the US JSON is upper-case, the API accepts both.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "DIMENSION" => Some(Self::Dimension),
            "MEASURE" => Some(Self::Measure),
            _ => None,
        }
    }
}

/// `dl_constants.enums.RawSQLLevel`: how much raw SQL a connection allows.
///
/// A connection that never sets the field is `off`, which is what the `us` storage schema
/// defaults to (`SubselectConnectionDataSchemaMixin`). The level is ordered: `subselect` also
/// permits a custom-SQL source, `dashsql` additionally permits an unwrapped query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawSqlLevel {
    #[default]
    Off,
    Subselect,
    Template,
    Dashsql,
}

impl RawSqlLevel {
    /// Lenient parse of the `data.raw_sql_level` string.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "" => Some(Self::Off),
            "subselect" => Some(Self::Subselect),
            "template" => Some(Self::Template),
            "dashsql" => Some(Self::Dashsql),
            _ => None,
        }
    }

    /// `is_raw_sql_level_subselect_allowed`: `subselect` and anything above it.
    pub fn allows_subselect(self) -> bool {
        self != Self::Off
    }
}

/// A physical source of the dataset (`DataSourceStrictSchema` + its avatar).
///
/// Three spellings reach the port and all are accepted ([`RawSource`]): the port's own
/// (`{"id", "table"}`, what the tests build), the one the dataset editor posts when it previews
/// an unsaved dataset (`{"id", "parameters": {"schema_name", "table_name"}, "raw_schema":
/// [{"name", "user_type"}]}`), and the stored-entry variant of the last one, where a column's
/// type is called `type`. A stored dataset is converted by `dl_us` instead — this is the path
/// of an *inline* dataset in a request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawSource")]
pub struct Source {
    /// Avatar id: what `field.source` refers to.
    pub id: String,
    /// Table reference, e.g. `public.sales`. Empty for a custom-SQL source.
    pub table: String,
    /// `DataSource.connection_id`: the `us` connection the table lives in.
    ///
    /// The dataset editor's inline content names it per source (the stored content names it once
    /// on the dataset); the transport resolves it into a DSN, the compiler never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// `SubselectDataSourceSpec.subsql`: the SQL of a "Custom SQL" source.
    ///
    /// `SubselectDataSource.get_sql_source` never names a table — it embeds the text as
    /// `(\n{subsql}\n) AS <alias>` — so the compiler needs it beside `table`, not instead.
    #[serde(default)]
    pub subselect: Option<String>,
    /// `origin.raw_schema`: the columns the source has.
    ///
    /// `DataSource.raw_schema` is what the `us` stores after reading the database schema, so
    /// it is the only place that says whether a `direct` field's column exists and what type
    /// it has. Python resolves both questions through it (`field.data_type` is the column's
    /// type), which is why a field pointing at a missing column is reported as broken — the
    /// demo's `Продажа, ₽` points at `Price` in a table that has no such column.
    #[serde(default)]
    pub columns: Vec<SourceColumn>,
}

/// Every shape a source is written in; [`Source`] keeps one.
#[derive(Debug, Clone, Deserialize)]
struct RawSource {
    id: String,
    #[serde(default)]
    table: Option<String>,
    #[serde(default)]
    subselect: Option<String>,
    /// `DataSource.connection_id`: the `us` connection the table lives in. The editor's inline
    /// dataset names it per source, not once per dataset.
    #[serde(default)]
    connection_id: Option<String>,
    /// `StandardSQLDataSourceSpec`: `table_name` / `schema_name` and, for custom SQL, `subsql`.
    #[serde(default)]
    parameters: Option<RawSourceParameters>,
    /// The port's own column list (`{"name", "data_type"}`).
    #[serde(default)]
    columns: Vec<SourceColumn>,
    /// `origin.raw_schema` (`{"name", "user_type"}`).
    #[serde(default)]
    raw_schema: Vec<RawSourceColumn>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawSourceParameters {
    #[serde(default)]
    table_name: Option<String>,
    #[serde(default)]
    schema_name: Option<String>,
    #[serde(default)]
    db_name: Option<String>,
    #[serde(default)]
    subsql: Option<String>,
}

/// One column of `origin.raw_schema` in a request: the user type is `user_type` there and
/// `type` in a stored entry.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawSourceColumn {
    #[serde(default)]
    name: String,
    #[serde(default, alias = "type")]
    user_type: Option<String>,
}

impl From<RawSource> for Source {
    fn from(raw: RawSource) -> Self {
        let parameters = raw.parameters.unwrap_or_default();
        let table = raw
            .table
            .filter(|table| !table.trim().is_empty())
            .or_else(|| table_of(&parameters))
            .unwrap_or_default();
        let subselect = raw
            .subselect
            .or_else(|| parameters.subsql.filter(|subsql| !subsql.trim().is_empty()));
        // A column whose type did not parse keeps `Unsupported` rather than being dropped: its
        // existence is still known, and the compiler only needs the name to resolve a `direct`
        // field (the same rule `dl_us` applies to a stored entry).
        let columns = if raw.columns.is_empty() {
            raw.raw_schema
                .iter()
                .filter(|column| !column.name.trim().is_empty())
                .map(|column| SourceColumn {
                    name: column.name.clone(),
                    data_type: column
                        .user_type
                        .as_deref()
                        .and_then(|text| UserDataType::parse(text).ok())
                        .unwrap_or(UserDataType::Unsupported),
                })
                .collect()
        } else {
            raw.columns
        };
        Self {
            id: raw.id,
            table,
            connection_id: raw.connection_id.filter(|id| !id.trim().is_empty()),
            subselect,
            columns,
        }
    }
}

/// `dl_constants.enums.JoinType`: how a relation joins its right avatar to its left one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinType {
    #[default]
    Inner,
    Left,
    Right,
    Full,
}

impl JoinType {
    /// The SQL keywords of the join, as `JoinType.name` spells them.
    pub fn sql(self) -> &'static str {
        match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::Right => "RIGHT JOIN",
            Self::Full => "FULL JOIN",
        }
    }

    /// Lenient parse of the `join_type` string (`inner`, `left`, ...).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "inner" => Some(Self::Inner),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// One side of a join condition (`ConditionPartDirect` / `ConditionPartResultField` /
/// `ConditionPartFormula`).
///
/// The dataset editor posts a `direct` part as `{"calc_mode": "direct", "source": "<column>"}`,
/// which is the only shape the demo uses. The other two spellings are accepted for completeness
/// and rejected by the compiler, which cannot build their SQL yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawJoinConditionPart", into = "RawJoinConditionPart")]
pub enum JoinConditionPart {
    /// A physical column of the part's own avatar.
    Direct(String),
    /// `field_id`: a field of the dataset, addressed by guid.
    ResultField(String),
    /// A formula over the dataset's fields.
    Formula(String),
}

impl JoinConditionPart {
    /// The physical column, for a `direct` part.
    pub fn column(&self) -> Option<&str> {
        match self {
            Self::Direct(column) => Some(column),
            _ => None,
        }
    }
}

/// Every shape a join condition part is written in; [`JoinConditionPart`] keeps one.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct RawJoinConditionPart {
    #[serde(default)]
    calc_mode: Option<String>,
    /// `ConditionPartDirect.source`.
    #[serde(default)]
    source: Option<String>,
    /// `ConditionPartResultField.field_id`.
    #[serde(default)]
    field_id: Option<String>,
    /// `ConditionPartFormula.formula`.
    #[serde(default)]
    formula: Option<String>,
}

impl From<RawJoinConditionPart> for JoinConditionPart {
    fn from(raw: RawJoinConditionPart) -> Self {
        match raw.calc_mode.as_deref().unwrap_or("direct") {
            "result_field" => Self::ResultField(raw.field_id.or(raw.source).unwrap_or_default()),
            "formula" => Self::Formula(raw.formula.or(raw.source).unwrap_or_default()),
            _ => Self::Direct(raw.source.or(raw.field_id).unwrap_or_default()),
        }
    }
}

/// The inverse of the above, so a [`Dataset`] round-trips through JSON unchanged.
impl From<JoinConditionPart> for RawJoinConditionPart {
    fn from(part: JoinConditionPart) -> Self {
        match part {
            JoinConditionPart::Direct(source) => Self {
                calc_mode: Some("direct".to_string()),
                source: Some(source),
                ..Self::default()
            },
            JoinConditionPart::ResultField(field_id) => Self {
                calc_mode: Some("result_field".to_string()),
                field_id: Some(field_id),
                ..Self::default()
            },
            JoinConditionPart::Formula(formula) => Self {
                calc_mode: Some("formula".to_string()),
                formula: Some(formula),
                ..Self::default()
            },
        }
    }
}

/// One `conditions[]` entry of an avatar relation: `left <operator> right`.
///
/// The stored `us` entry calls the sides `left_part`/`right_part` and the editor inline dataset
/// calls them `left`/`right`; both are read, and `condition_type`/`type` are ignored (only
/// binary conditions exist).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCondition {
    #[serde(default, alias = "left_part")]
    pub left: JoinConditionPart,
    #[serde(default, alias = "right_part")]
    pub right: JoinConditionPart,
    /// `eq`, `ne`, `gt`, `lt`, `gte`, `lte`. Anything else is rejected by the compiler.
    #[serde(default = "default_join_operator")]
    pub operator: String,
}

fn default_join_operator() -> String {
    "eq".to_string()
}

impl Default for JoinConditionPart {
    fn default() -> Self {
        Self::Direct(String::new())
    }
}

/// `dl_core.multisource.AvatarRelation`: a join between two source avatars of the dataset.
///
/// The query translator uses it twice: to decide which avatars the `FROM` clause joins, and to
/// render the `ON` clause of each join.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawAvatarRelation")]
pub struct AvatarRelation {
    pub id: String,
    pub left_avatar_id: String,
    pub right_avatar_id: String,
    #[serde(default)]
    pub join_type: JoinType,
    /// The conditions of the join, `AND`-ed together by the compiler.
    #[serde(default)]
    pub conditions: Vec<JoinCondition>,
}

/// Every shape an avatar relation is written in; [`AvatarRelation`] keeps one.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawAvatarRelation {
    #[serde(default)]
    id: String,
    #[serde(default)]
    left_avatar_id: Option<String>,
    #[serde(default)]
    right_avatar_id: Option<String>,
    #[serde(default)]
    join_type: Option<String>,
    #[serde(default)]
    conditions: Vec<JoinCondition>,
}

impl From<RawAvatarRelation> for AvatarRelation {
    fn from(raw: RawAvatarRelation) -> Self {
        Self {
            id: raw.id,
            left_avatar_id: raw.left_avatar_id.unwrap_or_default(),
            right_avatar_id: raw.right_avatar_id.unwrap_or_default(),
            join_type: raw
                .join_type
                .as_deref()
                .and_then(JoinType::parse)
                .unwrap_or_default(),
            conditions: raw.conditions,
        }
    }
}

impl AvatarRelation {
    /// Whether the relation has both ends and at least one condition, i.e. whether a `JOIN` can
    /// be built from it.
    pub fn is_usable(&self) -> bool {
        !self.left_avatar_id.is_empty()
            && !self.right_avatar_id.is_empty()
            && !self.conditions.is_empty()
    }
}

/// `public.orders` for `{schema_name: public, table_name: orders}`; `db_name` stands in when
/// the source carries no schema (`dl_us::dataset` resolves a stored entry the same way).
fn table_of(parameters: &RawSourceParameters) -> Option<String> {
    let table = parameters.table_name.as_deref()?.trim();
    if table.is_empty() {
        return None;
    }
    let schema = match parameters.schema_name.as_deref().map(str::trim) {
        Some(schema) if !schema.is_empty() => Some(schema),
        _ => match parameters.db_name.as_deref().map(str::trim) {
            Some(db_name) if !db_name.is_empty() => Some(db_name),
            _ => None,
        },
    };
    Some(match schema {
        Some(schema) => format!("{schema}.{table}"),
        None => table.to_string(),
    })
}

/// One column of a source's `raw_schema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceColumn {
    /// The physical column name (`amount`).
    pub name: String,
    /// Its user type (`float`); `Unsupported` when the schema did not parse.
    #[serde(default = "unsupported_column")]
    pub data_type: UserDataType,
}

fn unsupported_column() -> UserDataType {
    UserDataType::Unsupported
}

impl Source {
    /// A table source; the shape every test that is not about custom SQL needs.
    pub fn table(id: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            table: table.into(),
            connection_id: None,
            subselect: None,
            columns: Vec::new(),
        }
    }

    /// The schema entry of `column`, if the source carries a schema at all.
    pub fn column(&self, column: &str) -> Option<&SourceColumn> {
        self.columns.iter().find(|entry| entry.name == column)
    }
}

/// How a field is addressed by the request (`{type: "id"}` / `{type: "title"}` /
/// `{type: "measure_name"}` / `{type: "dimension_name"}` / `{type: "placeholder"}`).
///
/// `dl_api_lib.schemas.data.ItemRefSchema` registers exactly these five variants; the pseudo
/// ones carry no payload and are resolved by the legend, not by the dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FieldRef {
    Id {
        id: String,
    },
    Title {
        title: String,
    },
    /// `MeasureNameObjSpec` (`{"type": "measure_name"}`): the `Measure Names` pseudo-dimension
    /// a pivot table puts on one of its axes.
    #[serde(rename = "measure_name")]
    MeasureName,
    /// `DimensionNameObjSpec` (`{"type": "dimension_name"}`): the `Dimension Names`
    /// pseudo-dimension of a pivot table.
    #[serde(rename = "dimension_name")]
    DimensionName,
    /// `PlaceholderObjSpec`: not a dataset field at all.
    ///
    /// Its value is the `template` string of the role spec (a `''` literal in the SQL), which
    /// is how the charts engine labels a cell of a totals block:
    /// `{"ref": {"type": "placeholder"}, "role_spec": {"role": "template", "template": ""}}`.
    Placeholder,
}

/// The pseudo-field a [`FieldRef`] names, if it names one at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoField {
    MeasureName,
    DimensionName,
    Placeholder,
}

impl FieldRef {
    pub fn id(id: impl Into<String>) -> Self {
        Self::Id { id: id.into() }
    }

    pub fn title(title: impl Into<String>) -> Self {
        Self::Title {
            title: title.into(),
        }
    }

    pub fn placeholder() -> Self {
        Self::Placeholder
    }

    /// `false` for the pseudo-field, which no dataset can resolve.
    pub fn is_placeholder(&self) -> bool {
        matches!(self, Self::Placeholder)
    }

    /// `spec_is_field_name_pseudo_dimension`: a reference that no dataset field answers.
    pub fn pseudo(&self) -> Option<PseudoField> {
        match self {
            Self::MeasureName => Some(PseudoField::MeasureName),
            Self::DimensionName => Some(PseudoField::DimensionName),
            Self::Placeholder => Some(PseudoField::Placeholder),
            Self::Id { .. } | Self::Title { .. } => None,
        }
    }

    /// The display name of a pseudo reference (for error messages).
    pub fn pseudo_title(&self) -> &'static str {
        match self.pseudo() {
            Some(PseudoField::MeasureName) => "Measure Names",
            Some(PseudoField::DimensionName) => "Dimension Names",
            Some(PseudoField::Placeholder) => "Placeholder",
            None => "",
        }
    }
}

/// A dataset field (`FieldSchema` in `dl_api_lib`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub guid: String,
    pub title: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub initial_data_type: Option<UserDataType>,
    #[serde(default)]
    pub data_type: Option<UserDataType>,
    /// `cast` in the action JSON (`UpdateFieldBaseSchema.TYPE_FIELD_NAME`).
    #[serde(default)]
    pub cast: Option<UserDataType>,
    /// `DIMENSION` / `MEASURE` as stored in the dataset (`type` in the US JSON).
    #[serde(default, rename = "type")]
    pub field_type: Option<FieldType>,
    #[serde(default)]
    pub aggregation: Aggregation,
    #[serde(default)]
    pub calc_mode: CalcMode,
    /// Physical column name (`source` in the dataset/action JSON), for `calc_mode == direct`.
    #[serde(default)]
    pub source: String,
    /// Source avatar id (`avatar_id` in the dataset/action JSON).
    #[serde(default)]
    pub avatar_id: Option<String>,
    /// User-facing formula (`[Sales] * 2`), for `calc_mode == formula`.
    #[serde(default)]
    pub formula: String,
    /// Formula over field guids; takes precedence when present.
    #[serde(default)]
    pub guid_formula: String,
    /// Free-form description; carried through the updates like every other key.
    #[serde(default)]
    pub description: String,
    /// Opaque UI settings blob. Python caps its size, the port does not (yet).
    #[serde(default)]
    pub ui_settings: String,
    /// Default of a `parameter` field: the scalar of `{"type": ..., "value": ...}`.
    #[serde(default, deserialize_with = "de_typed_value")]
    pub default_value: Option<serde_json::Value>,
}

/// `{"type": "string", "value": "sales"}` -> `"sales"`; a plain scalar is kept as is.
pub fn de_typed_value<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.map(|value| match value {
        serde_json::Value::Object(mut object) if object.contains_key("value") => {
            object.remove("value").unwrap_or(serde_json::Value::Null)
        }
        other => other,
    }))
}

impl Field {
    /// A `parameter` field: constant per query, never a column.
    pub fn is_parameter(&self) -> bool {
        self.calc_mode == CalcMode::Parameter
    }

    /// The type the translator must see.
    ///
    /// `cast` is what `UpdateFieldBaseSchema` calls the type (`TYPE_FIELD_NAME`), while a
    /// stored dataset carries `data_type` / `initial_data_type`; all three are honoured.
    pub fn dtype(&self) -> Result<DataType, CoreError> {
        self.data_type
            .or(self.cast)
            .or(self.initial_data_type)
            .map(UserDataType::to_data_type)
            .ok_or_else(|| CoreError::MissingDataType {
                guid: self.guid.clone(),
            })
    }

    /// The formula to compile: the guid-based one when set, else the title-based one.
    pub fn formula_text(&self) -> &str {
        if !self.guid_formula.trim().is_empty() {
            &self.guid_formula
        } else {
            &self.formula
        }
    }

    /// Column name in the source table.
    pub fn physical_column(&self) -> &str {
        if self.source.trim().is_empty() {
            &self.title
        } else {
            &self.source
        }
    }
}

/// The dataset: sources plus fields, with lookup and validation.
///
/// Deserialized through [`RawDataset`] because a request may carry the `us` content, where a
/// field's `avatar_id` names a *source avatar* and the table lives on the *data source* that
/// avatar points at (`dl_us` joins the same two lists for a stored entry).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawDataset")]
pub struct Dataset {
    #[serde(default)]
    pub sources: Vec<Source>,
    /// `result_schema` in the `DatasetContentInternalSchema` JSON the `us` and the API use.
    #[serde(default, alias = "result_schema")]
    pub fields: Vec<Field>,
    /// `DatasetContentInternalSchema.data_export_forbidden`: the dataset forbids export.
    #[serde(default)]
    pub data_export_forbidden: bool,
    /// `source_collections[].origin.connection_id`: the `us` `connection` entry the tables live
    /// in. The query translator does not read it — the transport resolves it into a DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// `avatar_relations`: how the avatars of the dataset join each other.
    ///
    /// A query that reads one avatar needs no relation; one that reads several is translated into
    /// a `FROM` with a `JOIN` per relation, which is the only way a dataset with more than one
    /// table can answer at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub avatar_relations: Vec<AvatarRelation>,
    /// `source_avatars[].is_root`: the avatar a join tree starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_avatar_id: Option<String>,
    /// Row-level security rules (`rls` / `rls2`).
    #[serde(default)]
    pub rls: Rls,
}

/// The dataset as a request writes it: [`Dataset`] plus the `source_avatars` an inline `us`
/// content carries.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawDataset {
    #[serde(default)]
    sources: Vec<Source>,
    /// `DatasetContentInternalSchema.source_avatars`: the ids `field.avatar_id` refers to.
    #[serde(default)]
    source_avatars: Vec<RawSourceAvatar>,
    #[serde(default, alias = "result_schema")]
    fields: Vec<Field>,
    #[serde(default)]
    data_export_forbidden: bool,
    #[serde(default)]
    connection_id: Option<String>,
    /// `DatasetContentInternalSchema.avatar_relations`: the joins between the avatars.
    #[serde(default)]
    avatar_relations: Vec<AvatarRelation>,
    /// The root avatar, kept by a serialized [`Dataset`] whose `sources` are already joined.
    #[serde(default)]
    root_avatar_id: Option<String>,
    #[serde(default)]
    rls: Rls,
    #[serde(default)]
    rls2: std::collections::BTreeMap<String, Vec<RlsEntry>>,
}

/// One `source_avatars` entry (`dl_core.multisource.SourceAvatar`).
#[derive(Debug, Clone, Default, Deserialize)]
struct RawSourceAvatar {
    id: String,
    /// `source_collections[].id`: the data source the avatar reads from.
    #[serde(default)]
    source_id: Option<String>,
    /// The avatar's title (`public.orders`), which names the table when the data source has no
    /// parameters of its own.
    #[serde(default)]
    title: String,
    /// `SourceAvatar.is_root`: the avatar the join tree starts from.
    #[serde(default)]
    is_root: bool,
}

impl From<RawDataset> for Dataset {
    fn from(raw: RawDataset) -> Self {
        // The editor's inline dataset names the connection on each source, the stored content
        // once on the dataset; either way the transport needs one id to resolve a DSN.
        let connection_id = raw.connection_id.clone().or_else(|| {
            raw.sources
                .iter()
                .find_map(|source| source.connection_id.clone())
                .filter(|id| !id.trim().is_empty())
        });
        // A stored dataset arrives already joined by `dl_us`; an inline one has both lists, and
        // a field's `avatar_id` (`33810c50-…`) is the avatar, not the source (`a8eb19e0-…`) the
        // table and `raw_schema` belong to. Re-key every source by its avatar so
        // [`Dataset::source_for`] finds it.
        let sources = match raw.source_avatars.is_empty() {
            true => raw.sources,
            false => raw
                .source_avatars
                .iter()
                .filter_map(|avatar| {
                    let source = match avatar.source_id.as_deref() {
                        Some(source_id) => {
                            raw.sources.iter().find(|source| source.id == source_id)?
                        }
                        // No `source_id`: the avatar itself carries the origin (the shape
                        // `dl_us` reads), and one source means it can only be that one.
                        None => raw.sources.first()?,
                    };
                    let table = match source.table.is_empty() {
                        true => avatar.title.clone(),
                        false => source.table.clone(),
                    };
                    Some(Source {
                        id: avatar.id.clone(),
                        table,
                        connection_id: source.connection_id.clone(),
                        subselect: source.subselect.clone(),
                        columns: source.columns.clone(),
                    })
                })
                .collect(),
        };
        // Without a flag the first avatar is the root: `make_joined_from_for_avatars` does the
        // same when `root_avatar_id` is not set. A dataset whose `sources` are already joined
        // (a serialized [`Dataset`]) keeps `None`, and `root_source` falls back to the first
        // source — which is what this inference would answer anyway.
        let root_avatar_id = raw.root_avatar_id.or_else(|| {
            raw.source_avatars
                .iter()
                .find(|avatar| avatar.is_root)
                .or_else(|| raw.source_avatars.first())
                .map(|avatar| avatar.id.clone())
        });
        let mut rls = raw.rls;
        rls.extend_rls2(&raw.rls2);
        Self {
            sources,
            fields: raw.fields,
            data_export_forbidden: raw.data_export_forbidden,
            connection_id,
            avatar_relations: raw
                .avatar_relations
                .into_iter()
                .filter(AvatarRelation::is_usable)
                .collect(),
            root_avatar_id,
            rls,
        }
    }
}

impl Dataset {
    /// An exact key for the schema-dependent field metadata (types, casts, formulas, sources).
    ///
    /// This is deliberately content-based, not a pointer or a timestamp: a cached entry is reused
    /// only when every input that affects `field_types`/`field_data_types` is byte-for-byte the
    /// same. An `updates` action changes this string, so an edited dataset gets a fresh entry.
    pub fn schema_key(&self) -> String {
        use std::fmt::Write;
        let mut key = String::new();
        for field in &self.fields {
            let _ = write!(
                &mut key,
                "{}|{:?}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{};",
                field.guid,
                field.calc_mode,
                field.source,
                field.formula,
                field.cast,
                field.initial_data_type,
                field.aggregation,
                field.avatar_id,
                field.data_type,
                field
                    .default_value
                    .as_ref()
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            );
        }
        for source in &self.sources {
            let _ = write!(
                &mut key,
                "{}|{}|{}|{};",
                source.id,
                source.table,
                source.subselect.as_deref().unwrap_or_default(),
                source.connection_id.as_deref().unwrap_or_default()
            );
            for column in &source.columns {
                let _ = write!(&mut key, "{};", column.name);
            }
        }
        for relation in &self.avatar_relations {
            let _ = write!(
                &mut key,
                "{}|{}|{:?}|{:?};",
                relation.left_avatar_id,
                relation.right_avatar_id,
                relation.join_type,
                relation.conditions
            );
        }
        key
    }

    /// Whether any source is a Custom SQL subselect (`SubselectDataSource.get_sql_source`).
    ///
    /// Python refuses such a source when the connection's `raw_sql_level` is `off`; the
    /// transport checks this before compiling the query.
    pub fn uses_subselect(&self) -> bool {
        self.sources.iter().any(|source| {
            source
                .subselect
                .as_deref()
                .is_some_and(|sql| !sql.trim().is_empty())
        })
    }

    pub fn field_by_guid(&self, guid: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.guid == guid)
    }

    pub fn field_by_title(&self, title: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.title == title)
    }

    /// Resolve a request reference; titles are matched case-sensitively like `dl_core` does.
    pub fn resolve(&self, field_ref: &FieldRef) -> Option<&Field> {
        match field_ref {
            FieldRef::Id { id } => self.field_by_guid(id),
            FieldRef::Title { title } => self.field_by_title(title),
            // Pseudo fields (`Measure Names`, `Dimension Names`, a totals placeholder) name no
            // dataset column: the legend builds them, the query never selects them.
            FieldRef::MeasureName | FieldRef::DimensionName | FieldRef::Placeholder => None,
        }
    }

    /// The source a field reads from: its avatar, or the only one of the dataset.
    pub fn source_for(&self, field: &Field) -> Option<&Source> {
        match field.avatar_id.as_deref() {
            Some(avatar_id) => self.sources.iter().find(|source| source.id == avatar_id),
            None => self.sources.first(),
        }
    }

    /// The root avatar, i.e. where the `FROM` clause starts.
    ///
    /// `make_joined_from_for_avatars` falls back to the first avatar when the dataset names no
    /// root, and a dataset with no sources at all has none.
    pub fn root_source(&self) -> Option<&Source> {
        match self.root_avatar_id.as_deref() {
            Some(root) => self
                .sources
                .iter()
                .find(|source| source.id == root)
                .or_else(|| self.sources.first()),
            None => self.sources.first(),
        }
    }

    /// The relation that joins `right` to an avatar already in `joined`, if any.
    ///
    /// Relations are directed (`left_avatar_id` is the parent), but a join may be reached from
    /// either end, so a relation whose *left* end is `right` is returned with `swapped = true`:
    /// the compiler then renders its sides exchanged.
    pub fn relation_to<'a>(
        &'a self,
        joined: &[&Source],
        right: &Source,
    ) -> Option<(&'a AvatarRelation, bool)> {
        let is_joined = |id: &str| joined.iter().any(|source| source.id == id);
        for relation in &self.avatar_relations {
            if relation.right_avatar_id == right.id && is_joined(&relation.left_avatar_id) {
                return Some((relation, false));
            }
            if relation.left_avatar_id == right.id && is_joined(&relation.right_avatar_id) {
                return Some((relation, true));
            }
        }
        None
    }

    /// `DIMENSION` / `MEASURE` of a field: as stored, else derived from its aggregation.
    pub fn field_type(&self, field: &Field) -> FieldType {
        field
            .field_type
            .unwrap_or(if field.aggregation.is_aggregate() {
                FieldType::Measure
            } else {
                FieldType::Dimension
            })
    }
}

impl Dataset {
    /// `DatasetValidator`: the checks that decide whether a query may be built at all.
    ///
    /// Fail-fast: the *first* problem is the error. [`crate::validation::dataset_issues`] is the
    /// same walk, collecting; `/result-preflight` reports what it finds.
    pub fn validate(&self) -> Result<(), CoreError> {
        match crate::validation::dataset_issues(self).into_iter().next() {
            Some(issue) => Err(issue.error),
            None => Ok(()),
        }
    }

    /// `guid -> type` and `title -> type`, i.e. every name a formula may use.
    pub fn formula_field_types(&self) -> Result<FieldTypes, CoreError> {
        let mut types = FieldTypes::new();
        for field in &self.fields {
            let dtype = field.dtype()?;
            types.insert(field.guid.clone(), dtype.native());
            types.insert(field.title.clone(), dtype.native());
        }
        Ok(types)
    }

    /// `guid/title -> [alias, column]` so a formula turns into a qualified column.
    ///
    /// Parameters are *not* in the map: they are substituted as literals before the formula is
    /// parsed (see `dl_query::compiler::Resolver::substitute_parameters`).
    pub fn formula_field_names(&self, alias: &str) -> Result<FieldNames, CoreError> {
        let mut names = FieldNames::new();
        for field in self.fields.iter().filter(|field| !field.is_parameter()) {
            let path =
                FieldName::Path(vec![alias.to_string(), field.physical_column().to_string()]);
            names.insert(field.guid.clone(), path.clone());
            names.insert(field.title.clone(), path);
        }
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dataset editor previews an unsaved dataset, and its sources are the `us` content:
    /// `parameters.table_name` instead of `table`, `raw_schema` instead of `columns`, and a
    /// column's type under `user_type`. All of it has to resolve to the same [`Source`].
    #[test]
    fn a_source_in_the_us_shape_resolves_its_table_and_columns() {
        let source: Source = serde_json::from_value(serde_json::json!({
            "id": "a8eb19e0-8ace-11ee-affd-8fdf6cc2041c",
            "source_type": "PG_TABLE",
            "managed_by": "user",
            "title": "orders",
            "parameters": {"schema_name": "public", "table_name": "orders"},
            "raw_schema": [
                {"name": "amount", "user_type": "float", "nullable": false},
                {"name": "order_id", "type": "string"},
                {"name": "  ", "user_type": "string"},
            ]
        }))
        .unwrap();
        assert_eq!(source.table, "public.orders");
        assert_eq!(source.subselect, None);
        // A column without a name is dropped; a named one keeps its type even when it did not
        // parse, and `type` is the stored spelling of `user_type`.
        assert_eq!(source.columns.len(), 2);
        assert_eq!(source.columns[0].name, "amount");
        assert_eq!(source.columns[0].data_type, UserDataType::Float);
        assert_eq!(source.columns[1].data_type, UserDataType::String);
    }

    /// A field's `avatar_id` is the avatar, while the table and `raw_schema` belong to the data
    /// source it points at — the editor's inline dataset has both lists.
    #[test]
    fn an_inline_dataset_keys_its_sources_by_avatar() {
        let dataset: Dataset = serde_json::from_value(serde_json::json!({
            "result_schema": [
                {"guid": "f_amount", "title": "amount", "avatar_id": "avatar-1",
                 "source": "amount", "calc_mode": "direct", "data_type": "float",
                 "type": "MEASURE"}
            ],
            "sources": [{
                "id": "source-1",
                "parameters": {"schema_name": "public", "table_name": "orders"},
                "raw_schema": [{"name": "amount", "user_type": "float"}]
            }],
            "source_avatars": [
                {"id": "avatar-1", "source_id": "source-1", "title": "public.orders"}
            ]
        }))
        .unwrap();
        let source = dataset
            .source_for(&dataset.fields[0])
            .expect("the avatar names its source");
        assert_eq!(source.table, "public.orders");
        assert_eq!(
            source.column("amount").unwrap().data_type,
            UserDataType::Float
        );
    }

    /// The editor posts `avatar_relations` with `left`/`right` parts, the stored `us` entry with
    /// `left_part`/`right_part`; both describe the same join.
    #[test]
    fn avatar_relations_are_read_in_both_shapes() {
        let inline: Dataset = serde_json::from_value(serde_json::json!({
            "result_schema": [],
            "sources": [{"id": "source-1", "parameters": {"schema_name": "public", "table_name": "orders"}}],
            "source_avatars": [
                {"id": "avatar-1", "source_id": "source-1", "title": "public.orders", "is_root": true}
            ],
            "avatar_relations": [{
                "id": "rel-1",
                "left_avatar_id": "avatar-1",
                "right_avatar_id": "avatar-2",
                "join_type": "left",
                "conditions": [{
                    "left": {"calc_mode": "direct", "source": "category"},
                    "right": {"calc_mode": "direct", "source": "category"},
                    "operator": "eq",
                    "type": "binary",
                }],
            }],
        }))
        .unwrap();
        assert_eq!(inline.root_avatar_id.as_deref(), Some("avatar-1"));
        assert_eq!(inline.avatar_relations.len(), 1);
        let relation = &inline.avatar_relations[0];
        assert_eq!(relation.join_type, JoinType::Left);
        assert_eq!(relation.left_avatar_id, "avatar-1");
        assert_eq!(
            relation.conditions[0].right,
            JoinConditionPart::Direct("category".to_string())
        );

        let stored: Dataset = serde_json::from_value(serde_json::json!({
            "result_schema": [],
            "sources": [
                {"id": "avatar-1", "table": "public.orders"},
                {"id": "avatar-2", "table": "public.managers"},
            ],
            "avatar_relations": [{
                "id": "rel-1",
                "left_avatar_id": "avatar-1",
                "right_avatar_id": "avatar-2",
                "join_type": "inner",
                "conditions": [{
                    "left_part": {"calc_mode": "direct", "source": "category_name_level_1"},
                    "right_part": {"calc_mode": "direct", "source": "category_name_level_1"},
                    "condition_type": "binary",
                    "operator": "eq",
                }],
            }],
        }))
        .unwrap();
        // No avatars means no `is_root` flag: the first source is the root.
        assert_eq!(stored.root_avatar_id, None);
        assert_eq!(
            stored.root_source().map(|source| source.id.as_str()),
            Some("avatar-1")
        );
        assert_eq!(stored.avatar_relations[0].join_type, JoinType::Inner);
        // A relation without both ends cannot become a JOIN and is dropped.
        let broken: Dataset = serde_json::from_value(serde_json::json!({
            "avatar_relations": [{"id": "rel-2", "left_avatar_id": "avatar-1", "conditions": []}],
        }))
        .unwrap();
        assert!(broken.avatar_relations.is_empty());
    }

    /// The editor's inline dataset names the connection per source; without it the preview asks
    /// the executor for a DSN and gets none, which is "answering without rows".
    #[test]
    fn the_connection_of_an_inline_dataset_comes_from_its_sources() {
        let dataset: Dataset = serde_json::from_value(serde_json::json!({
            "result_schema": [],
            "sources": [{
                "id": "source-1",
                "connection_id": "gz2tnvb9zdxm5",
                "parameters": {"schema_name": "public", "table_name": "orders"},
            }],
            "source_avatars": [{"id": "avatar-1", "source_id": "source-1", "title": "public.orders"}],
        }))
        .unwrap();
        assert_eq!(dataset.connection_id.as_deref(), Some("gz2tnvb9zdxm5"));
        assert_eq!(
            dataset.sources[0].connection_id.as_deref(),
            Some("gz2tnvb9zdxm5")
        );

        // The stored content names it once on the dataset, and that one wins.
        let stored: Dataset = serde_json::from_value(serde_json::json!({
            "connection_id": "stored",
            "sources": [{"id": "src", "table": "public.sales", "connection_id": "source-level"}],
        }))
        .unwrap();
        assert_eq!(stored.connection_id.as_deref(), Some("stored"));
    }

    /// The port's own shape keeps working, and a custom-SQL source still carries its text.
    #[test]
    fn the_compact_source_shape_and_custom_sql_still_deserialize() {
        let table: Source = serde_json::from_value(serde_json::json!({
            "id": "src",
            "table": "public.sales",
            "columns": [{"name": "sales", "data_type": "integer"}]
        }))
        .unwrap();
        assert_eq!(table.table, "public.sales");
        assert_eq!(table.columns[0].data_type, UserDataType::Integer);

        let subselect: Source = serde_json::from_value(serde_json::json!({
            "id": "src",
            "parameters": {"subsql": "SELECT 1 AS one"}
        }))
        .unwrap();
        assert_eq!(subselect.table, "");
        assert_eq!(subselect.subselect.as_deref(), Some("SELECT 1 AS one"));
    }
    #[test]
    fn raw_sql_levels_parse_and_the_subselect_gate_is_ordered() {
        assert_eq!(RawSqlLevel::parse("off"), Some(RawSqlLevel::Off));
        assert_eq!(
            RawSqlLevel::parse("SUBSELECT"),
            Some(RawSqlLevel::Subselect)
        );
        assert_eq!(RawSqlLevel::parse("dashsql"), Some(RawSqlLevel::Dashsql));
        assert_eq!(RawSqlLevel::parse("nonsense"), None);
        assert!(!RawSqlLevel::Off.allows_subselect());
        assert!(RawSqlLevel::Subselect.allows_subselect());
        assert!(RawSqlLevel::Template.allows_subselect());
        assert!(RawSqlLevel::Dashsql.allows_subselect());
        assert_eq!(RawSqlLevel::default(), RawSqlLevel::Off);
    }

    #[test]
    fn a_dataset_knows_whether_it_uses_custom_sql() {
        let mut dataset = Dataset::default();
        assert!(!dataset.uses_subselect());
        dataset.sources.push(Source {
            id: "src".to_string(),
            table: String::new(),
            connection_id: None,
            subselect: Some("SELECT 1".to_string()),
            columns: Vec::new(),
        });
        assert!(dataset.uses_subselect());
        dataset.sources[0].subselect = Some("   ".to_string());
        assert!(!dataset.uses_subselect());
    }

    #[test]
    fn the_schema_key_changes_when_a_field_expression_changes() {
        let mut left = Dataset::default();
        left.fields.push(Field {
            guid: "f".to_string(),
            formula: "[a] + 1".to_string(),
            ..Field::default()
        });
        let mut right = left.clone();
        right.fields[0].formula = "[a] + 2".to_string();
        assert_ne!(left.schema_key(), right.schema_key());
        assert_eq!(left.schema_key(), left.schema_key());
    }
}
