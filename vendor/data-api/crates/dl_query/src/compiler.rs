//! Dataset + request spec -> SQL.

use std::collections::{HashMap, HashSet};

use dfl_native::{FieldName, Item};
use dl_core::{
    Aggregation, CalcMode, CoreError, DataType, Dataset, Field, FieldType, UserDataType,
};

use crate::identity;
use crate::literal::{Dialect, LiteralError};
use crate::plan::{Select, SelectExpr, Source};
use crate::{Direction, FilterOperation, FilterSpec, ParameterValueSpec, QuerySpec, Role};

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("Unknown field {0}")]
    UnknownField(String),
    /// `dl_formula`'s `UnknownFieldInFormulaError`: a formula references a name the dataset does
    /// not have. The reference answers `ERR.DS_API.FORMULA.UNKNOWN_FIELD_IN_FORMULA`.
    #[error("Unknown field found in formula: {0}")]
    UnknownFieldInFormula(String),
    #[error("the request touches {0} sources; no avatar relation joins them together")]
    MultipleSources(usize),
    #[error("field {guid} has no source table")]
    MissingSource { guid: String },
    #[error("formula field {guid} did not translate: {reason}")]
    Formula { guid: String, reason: String },
    #[error("filter {operation} on {guid} expects {expected}, got {actual}")]
    BadValueCount {
        guid: String,
        operation: &'static str,
        expected: &'static str,
        actual: usize,
    },
    /// `dl_query_processing.exc.MeasureFilterUnsupportedError`.
    #[error("measure filter is unsupported for this type of query: {guid}")]
    MeasureFilterUnsupported { guid: String },
    /// A parameter field with neither a request value nor a `default_value`.
    #[error("parameter {guid} has no value")]
    MissingParameter { guid: String },
    /// `order_by` over an item that is in the legend but not in the `SELECT` list.
    #[error("{0} is not selected, so the request cannot order by it")]
    NotSelected(String),
    #[error(transparent)]
    Literal(#[from] LiteralError),
    #[error("dataset is invalid: {0}")]
    Invalid(String),
    /// RLS cannot be applied (no authenticated subject, or a rule names a missing field).
    #[error("row-level security: {0}")]
    Rls(String),
}

/// One legend item of the result, with everything the row decoder needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    /// Position in the legend; what a row's `legend` array refers to.
    pub legend_item_id: i64,
    pub guid: String,
    pub title: String,
    pub data_type: DataType,
    pub aggregation: Aggregation,
    pub role: Role,
    /// `false` for `info`/`template`/`parameter` items: they are in the legend but not in the
    /// SQL and therefore have no value in a row.
    pub in_result_set: bool,
}

/// A compiled `SELECT` plus the metadata to decode its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledQuery {
    pub sql: String,
    pub columns: Vec<OutputColumn>,
    /// `GROUP BY` was emitted (there is at least one aggregate).
    pub grouped: bool,
}

fn unknown(field_ref: &dl_core::FieldRef) -> QueryError {
    QueryError::UnknownField(name_of(field_ref))
}

/// The alias every query gives its source.
///
/// [`field_types`] compiles a field's expression only to see whether it aggregates; the SQL it
/// renders is thrown away, so it uses the same aliases a real query would.
const FIRST_ALIAS: &str = "t0";

/// The name a request used for a field, for error messages.
fn name_of(field_ref: &dl_core::FieldRef) -> String {
    match field_ref {
        dl_core::FieldRef::Id { id } => id.clone(),
        dl_core::FieldRef::Title { title } => title.clone(),
        dl_core::FieldRef::MeasureName => "Measure Names".to_string(),
        dl_core::FieldRef::DimensionName => "Dimension Names".to_string(),
        dl_core::FieldRef::Placeholder => "placeholder".to_string(),
    }
}

/// `SUM([X])` / `MIN([X])` / `MAX([X])` where `[X]` is already an aggregate: the outer call has
/// one value per group, so on a distributive aggregate it is the identity.
///
/// PostgreSQL and ClickHouse reject the nested form (`aggregate function calls cannot be
/// nested`), and `dl_query_processing` reaches the same number by lifting the argument into a
/// sub-query and aggregating its column above it. Dropping the outer call keeps the same value
/// and keeps the query valid: the rewrite matters for forms such as
/// `SUM([Revenue]) - SUM([Cost]) - SUM([Ads])`, where the referenced fields are formulas
/// over `sum(...)`.
fn collapse_redundant_aggregates(item: &mut Item, aggregated_names: &HashSet<String>) {
    match item {
        Item::Unary(_, child) => collapse_redundant_aggregates(child, aggregated_names),
        Item::Binary(_, left, right) => {
            collapse_redundant_aggregates(left, aggregated_names);
            collapse_redundant_aggregates(right, aggregated_names);
        }
        Item::In { value, list, .. } => {
            collapse_redundant_aggregates(value, aggregated_names);
            for element in list {
                collapse_redundant_aggregates(element, aggregated_names);
            }
        }
        Item::Is { value, .. } => collapse_redundant_aggregates(value, aggregated_names),
        Item::Between {
            value, low, high, ..
        } => {
            collapse_redundant_aggregates(value, aggregated_names);
            collapse_redundant_aggregates(low, aggregated_names);
            collapse_redundant_aggregates(high, aggregated_names);
        }
        Item::Case {
            operand,
            branches,
            else_,
        } => {
            if let Some(operand) = operand {
                collapse_redundant_aggregates(operand, aggregated_names);
            }
            for (when, then) in branches {
                collapse_redundant_aggregates(when, aggregated_names);
                collapse_redundant_aggregates(then, aggregated_names);
            }
            if let Some(else_) = else_ {
                collapse_redundant_aggregates(else_, aggregated_names);
            }
        }
        Item::Call { args, .. } => {
            for arg in args {
                collapse_redundant_aggregates(arg, aggregated_names);
            }
        }
        _ => {}
    }
    let drop_call = match item {
        Item::Call {
            name,
            args,
            clauses,
        } => {
            args.len() == 1
                && clauses.is_empty()
                && (name.eq_ignore_ascii_case("sum")
                    || name.eq_ignore_ascii_case("min")
                    || name.eq_ignore_ascii_case("max"))
                && is_aggregate_expression(&args[0], aggregated_names)
        }
        _ => false,
    };
    if drop_call {
        if let Item::Call { mut args, .. } = std::mem::replace(item, Item::Null) {
            if let Some(arg) = args.drain(..).next() {
                *item = arg;
            }
        }
    }
}

/// Whether the substituted subtree computes one value per group: an aggregate call, or
/// arithmetic/casts over aggregates and constants. A bare column is not (`false`), which keeps
/// the rewrite away from `SUM([agg] + [Region])`-shaped expressions the query cannot group.
fn is_aggregate_expression(item: &Item, aggregated_names: &HashSet<String>) -> bool {
    match item {
        Item::Field(name) => aggregated_names.contains(name.as_ref()),
        Item::Num { .. } | Item::Str(_) | Item::Date { .. } | Item::Bool(_) | Item::Null => true,
        Item::Unary(_, child) => is_aggregate_expression(child, aggregated_names),
        Item::Binary(_, left, right) => {
            is_aggregate_expression(left, aggregated_names)
                && is_aggregate_expression(right, aggregated_names)
        }
        Item::Call {
            name,
            args,
            clauses,
        } => {
            let props = dfl_native::func_props::call_props(name);
            let window = props.can_be_window && (!props.can_be_nonwindow || clauses.has_grouping());
            if props.can_be_aggregate && !window && clauses.is_empty() {
                true
            } else {
                // `CAST(sum(x) AS ...)` / `IFNULL(sum(x), 0)` are aggregates over aggregates.
                args.iter()
                    .all(|arg| is_aggregate_expression(arg, aggregated_names))
            }
        }
        _ => false,
    }
}

/// Position of `field_ref` among the *selected* columns: what `ORDER BY <n>` must point at.
///
/// Legend-only items (`info`, `template`, `parameter`) are skipped: they are not in the
/// `SELECT` list, so they cannot shift the position of a column after them.
fn position_of(columns: &[OutputColumn], field_ref: &dl_core::FieldRef) -> Option<usize> {
    columns
        .iter()
        .filter(|column| column.in_result_set)
        .position(|column| match field_ref {
            dl_core::FieldRef::Id { id } => &column.guid == id,
            dl_core::FieldRef::Title { title } => &column.title == title,
            // Pseudo fields are no columns: a placeholder is a constant the decoder fills in,
            // `Measure Names`/`Dimension Names` are pivot dimensions the legend adds.
            dl_core::FieldRef::MeasureName
            | dl_core::FieldRef::DimensionName
            | dl_core::FieldRef::Placeholder => false,
        })
}

/// Whether the item is in the legend at all (with a non-`SELECT` role, i.e. `info`/`template`).
fn in_legend_only(columns: &[OutputColumn], field_ref: &dl_core::FieldRef) -> bool {
    columns.iter().any(|column| {
        !column.in_result_set
            && match field_ref {
                dl_core::FieldRef::Id { id } => &column.guid == id,
                dl_core::FieldRef::Title { title } => &column.title == title,
                dl_core::FieldRef::MeasureName
                | dl_core::FieldRef::DimensionName
                | dl_core::FieldRef::Placeholder => false,
            }
    })
}

/// `FormulaCompiler._CAST_FUNCTIONS`: the formula function that casts to a user type.
///
/// Python builds the table from `_SUPPORTED_CASTS_FUNCTIONS` and maps every other type to
/// `__UNSUPPORTED_CAST_TO_<TYPE>__`, which no registry knows — the formula then fails to
/// translate, exactly as it does in the reference service.
fn cast_function_name(cast: UserDataType) -> String {
    match cast {
        UserDataType::Boolean => "bool".to_string(),
        UserDataType::Date => "date".to_string(),
        UserDataType::Datetime => "datetime".to_string(),
        UserDataType::Genericdatetime => "genericdatetime".to_string(),
        UserDataType::Float => "float".to_string(),
        UserDataType::Integer => "int".to_string(),
        UserDataType::Geopoint => "geopoint".to_string(),
        UserDataType::Geopolygon => "geopolygon".to_string(),
        UserDataType::String => "str".to_string(),
        UserDataType::Markup => "markup".to_string(),
        other => format!("__UNSUPPORTED_CAST_TO_{}__", other.name().to_uppercase()),
    }
}

/// `FormulaCompiler._apply_cast`: the cast function to wrap the expression in, if any.
///
/// `current_dtype` is the type of the formula *after* the references were substituted, and
/// `cast` is the field's `cast` — the user type the dataset declares. `None` means the
/// expression already has that type (or the field does not declare one), and a `NULL`
/// expression is never cast: `if current_dtype != DataType.NULL`.
fn cast_function(expr_type: DataType, cast: Option<UserDataType>) -> Option<String> {
    if expr_type.native().base() == dfl_native::DataType::Null {
        return None;
    }
    let cast = cast?;
    let current = UserDataType::from_data_type(expr_type);
    (cast != current).then(|| cast_function_name(cast))
}

/// `QueryFormalizer.validate_select_field`: a request validates the fields **it names**, not
/// the whole dataset.
///
/// `dl_api_lib` checks that a selected `direct` field's `avatar_id` resolves only when the
/// field is in a block (`_avatar_exists`, then `UnknownReferencedAvatarError`), so a stale
/// avatar on a field nothing selects is not an error. The demo's `Charts: Pie chart` sends
/// exactly that in `updates` — its `52615f20` measure still points at an avatar the dataset
/// no longer has — and Python answers 200 while validating the whole dataset would 400.
fn validate_query_fields(dataset: &Dataset, spec: &QuerySpec) -> Result<(), QueryError> {
    let mut used: HashSet<&str> = HashSet::new();
    for field_ref in spec
        .select
        .iter()
        .map(|item| &item.field_ref)
        .chain(spec.filters.iter().map(|filter| &filter.field_ref))
        .chain(spec.order_by.iter().map(|order| &order.field_ref))
    {
        if let Some(field) = dataset.resolve(field_ref) {
            used.insert(field.guid.as_str());
        }
    }
    // A dataset-level problem (no fields at all) has no owner and always fails.
    let first = dl_core::validation::dataset_issues(dataset)
        .into_iter()
        .find(|issue| match issue.field.as_deref() {
            None => true,
            Some(guid) => used.contains(guid),
        });
    match first {
        Some(issue) => Err(QueryError::Core(issue.error)),
        None => Ok(()),
    }
}

fn aggregate_sql(expr: String, aggregation: Aggregation) -> String {
    match aggregation {
        Aggregation::Countunique => format!("COUNT(DISTINCT {expr})"),
        _ => {
            let name = aggregation.sql_name().unwrap_or("SUM");
            format!("{name}({expr})")
        }
    }
}

/// Compile `spec` against `dataset` for `dialect`.
///
/// This is where `data-api` decides what the query looks like: which fields are selected,
/// how they are aggregated, what the `WHERE` clause is. Everything that is a *formula*
/// goes through `dfl_native`, i.e. through the `dl_formula` registry.
/// Add the caller's RLS restrictions to a query spec.
///
/// Runs inside [`compile`], [`compile_distinct`] and [`compile_range`], so every SQL statement
/// the service builds — result, totals, pivot, preview, values — gets the same filters. A
/// dataset with rules but no authenticated subject is an error, never an unrestricted query.
fn apply_rls<'a>(dataset: &Dataset, spec: &'a QuerySpec) -> Result<Option<QuerySpec>, QueryError> {
    if dataset.rls.is_empty() {
        return Ok(None);
    }
    let Some(identity) = identity::current() else {
        return Err(QueryError::Rls(
            "no authenticated user to evaluate the dataset rules".to_string(),
        ));
    };
    let restrictions = dataset
        .rls
        .restrictions(&identity.user_id, &identity.groups);
    if restrictions.is_empty() {
        return Ok(None);
    }
    let mut spec = spec.clone();
    for (field_guid, values) in restrictions {
        if dataset.field_by_guid(&field_guid).is_none() {
            return Err(QueryError::Rls(format!(
                "the rule names field {field_guid}, which the dataset does not have"
            )));
        }
        spec.filters.push(FilterSpec {
            field_ref: dl_core::FieldRef::Id { id: field_guid },
            operation: FilterOperation::In,
            values: values.into_iter().map(serde_json::Value::String).collect(),
        });
    }
    Ok(Some(spec))
}

pub fn compile(
    dataset: &Dataset,
    spec: &QuerySpec,
    dialect: Dialect,
) -> Result<CompiledQuery, QueryError> {
    let rls_spec = apply_rls(dataset, spec)?;
    let spec = rls_spec.as_ref().unwrap_or(spec);
    validate_query_fields(dataset, spec)?;
    if spec.select.is_empty() {
        return Err(QueryError::Invalid(
            "the request selects no fields".to_string(),
        ));
    }

    let mut resolver = Resolver::new(dataset, spec, dialect)?;

    let mut items: Vec<SelectExpr> = Vec::with_capacity(spec.select.len());
    let mut columns: Vec<OutputColumn> = Vec::with_capacity(spec.select.len());
    let mut group_exprs: Vec<String> = Vec::new();
    let mut has_measures = false;
    for item in &spec.select {
        let field = dataset
            .resolve(&item.field_ref)
            .ok_or_else(|| unknown(&item.field_ref))?;
        // `dl_api_lib` keeps `info`, `order_by`, `filter` and `parameter` items in the legend
        // only: they shape the request (or the pivot), never the `SELECT` list.
        let in_result_set = item.role.is_selectable();
        let mut dtype = field.dtype()?;
        if in_result_set {
            let leaves_before = resolver.leaves.len();
            let FieldExpr {
                sql: expr,
                data_type,
                aggregated,
                is_constant,
                window,
                ..
            } = resolver.expression(field)?;
            dtype = data_type;
            // `_make_totals` builds its query from the measures alone: a measure whose formula
            // needs a LOD/window fork (`[Sales] / sum([Sales] TOTAL)`) has no such fork there,
            // and the reference renders it as `NULL` instead of a made-up value.
            let fork_in_totals = spec.totals
                && resolver.leaves[leaves_before..]
                    .iter()
                    .any(|leaf| leaf.external.is_some());
            if fork_in_totals {
                // The leaves belong to the item that became `NULL`: dropping them keeps a fork
                // nothing refers to out of the totals query.
                resolver.leaves.truncate(leaves_before);
                items.push(SelectExpr {
                    sql: "NULL".to_string(),
                    alias: dialect.quote_ident(&field.guid),
                });
                columns.push(OutputColumn {
                    legend_item_id: columns.len() as i64,
                    guid: field.guid.clone(),
                    title: field.title.clone(),
                    data_type: dtype,
                    aggregation: field.aggregation,
                    role: item.role,
                    in_result_set,
                });
                continue;
            }
            // A dimension is one column of the base query, even when it is a formula over other
            // fields: `CAST(DATE_PART('year', [date]) AS BIGINT)` selects and groups by the cast,
            // not by the raw date it was built from.
            let expr = if aggregated {
                expr
            } else {
                resolver
                    .collapse(FieldExpr {
                        sql: expr,
                        data_type,
                        aggregated,
                        aggregate_expression: false,
                        is_constant,
                        window,
                    })
                    .sql
            };
            // `_make_tree_wrapper`: a `tree` item selects `slice(expr, 1, level)`, i.e. the first
            // `level` entries of the array it is built from. `dl_formula` applies the very same
            // node in Python (`ArrayPrefixSelectWrapperSpec`), so the template is the registry's.
            let expr = match item.role {
                Role::Tree => resolver.slice_wrapper(&expr, data_type, item.level)?,
                _ => expr,
            };
            // `make_group_by_specs` groups by every selected *dimension* — and only by a
            // dimension: a measure with `aggregation: none` is not in the `GROUP BY`. A
            // constant (`'Afeka'`, a parameter) is not one either: PostgreSQL rejects
            // `GROUP BY ('Afeka')` and `dl_formula.slicing` treats constant dimensions as
            // neutral.
            if !aggregated
                && !is_constant
                && dataset.field_type(field) == dl_core::FieldType::Dimension
            {
                group_exprs.push(expr.clone());
            }
            // `GroupByPolicy.if_measures` counts the *selected* measures of the dataset, which is
            // what the preview's grouping depends on.
            has_measures |= dataset.field_type(field) == dl_core::FieldType::Measure;
            items.push(SelectExpr {
                sql: expr,
                alias: dialect.quote_ident(&field.guid),
            });
        }
        columns.push(OutputColumn {
            legend_item_id: columns.len() as i64,
            guid: field.guid.clone(),
            title: field.title.clone(),
            data_type: dtype,
            aggregation: field.aggregation,
            role: item.role,
            in_result_set,
        });
    }

    // `dl_query_processing.translation.flat_translator`: a filter whose expression contains an
    // aggregation becomes `HAVING`, everything else becomes `WHERE`.
    //
    // `_generate_tree_filter_items`: a `tree` item is cut down to its first `level` entries by a
    // `LENGTE` filter on the item itself, so the filter is added here, after the request's own
    // ones. Python generates it in the legend formalizer and it ends up in the same place.
    let mut filters: Vec<&FilterSpec> = spec.filters.iter().collect();
    let tree_filters: Vec<FilterSpec> = spec
        .select
        .iter()
        .filter(|item| item.role == Role::Tree)
        .filter_map(|item| {
            let level = item.level?;
            Some(FilterSpec {
                field_ref: item.field_ref.clone(),
                operation: FilterOperation::Lengte,
                values: vec![serde_json::json!(level)],
            })
        })
        .collect();
    filters.extend(tree_filters.iter());
    let mut where_parts: Vec<String> = Vec::with_capacity(filters.len());
    let mut having_parts: Vec<String> = Vec::new();
    // A filter on a window function is neither a `WHERE` (the window is computed after the
    // grouping) nor a `HAVING` (no dialect takes a window there): the query is wrapped in a
    // sub-query that selects the window, and the filter is applied to that.
    let mut window_parts: Vec<FilterPart> = Vec::new();
    for filter in filters {
        let Some(part) = resolver.filter(filter, spec.ignore_nonexistent_filters)? else {
            continue;
        };
        if part.window {
            window_parts.push(part);
        } else if part.having {
            having_parts.push(part.condition);
        } else {
            where_parts.push(part.condition);
        }
    }

    let from = resolver.from_item()?;
    // `GroupByPolicy`: `/result` and `/pivot` force the grouping, so a plain list of dimensions
    // comes back unique (the Cohorts dataset selects only dimensions, and without the `GROUP BY`
    // its rows were duplicated); the preview groups only when it selects a measure
    // (`if_measures`), because a dataset of dimensions is meant to be shown row by row; and
    // `disable_group_by` in a request turns it off.
    let group_by_wanted = if spec.disable_group_by {
        false
    } else if spec.group_by_if_measures {
        has_measures
    } else {
        // `force`: always group by the selected dimensions.
        true
    };
    let grouped = group_by_wanted && !group_exprs.is_empty();

    let mut order_by: Vec<String> = Vec::with_capacity(spec.order_by.len());
    for order in &spec.order_by {
        let position = position_of(&columns, &order.field_ref).ok_or_else(|| {
            if in_legend_only(&columns, &order.field_ref) {
                QueryError::NotSelected(name_of(&order.field_ref))
            } else {
                unknown(&order.field_ref)
            }
        })?;
        order_by.push(format!(
            "{} {}",
            position + 1,
            match order.direction {
                Direction::Asc => "ASC",
                Direction::Desc => "DESC",
            }
        ));
    }

    // `QueryForkQuerySplitter` applies a filter built on a window in the query *above* the
    // window fork: the planning layer needs the expression and the column it gets there.
    let window_filters: Vec<crate::fork::WindowFilter> = window_parts
        .into_iter()
        .map(|part| crate::fork::WindowFilter {
            expr: part.expr,
            alias: part.alias,
            condition: part.condition,
        })
        .collect();
    // A window a window is built from (`MAX([precalc] AMONG [Month])` with a window inside
    // `[precalc]`) is not one `SELECT`: `WinFuncQuerySplitter` keeps every window function as the
    // topmost node of its query, so the inner one becomes a query of its own.
    if resolver.leaves.iter().any(|leaf| leaf.window) {
        let dimensions = resolver.window_context()?.group_dims;
        dfl_native::fork_nested_windows(&mut resolver.leaves, &dimensions);
    }
    let plan = crate::fork::split(
        Select {
            items,
            from: Source::Table(from),
            joins: Vec::new(),
            filters: where_parts,
            group_by: if grouped { group_exprs } else { Vec::new() },
            having: having_parts,
            order_by,
            limit: spec.limit.map(|value| value as usize),
            offset: spec.offset.map(|value| value as usize),
            ctes: Vec::new(),
        },
        &std::mem::take(&mut resolver.leaves),
        &window_filters,
        dialect,
    )?;

    Ok(CompiledQuery {
        sql: plan.render(),
        columns,
        grouped,
    })
}

/// `data_type` for formula fields the dataset carries without one (a freshly added field in
/// the editor: the UI sends `add_field` with a formula and no type).
///
/// `DatasetValidator` in the Python control-api compiles the formula first and then stores the
/// inferred type, which is why the structural pass sees a typed field. The data-api port only
/// stores what it was given, so the editor's validation route has to infer the type itself:
/// build a resolver over a copy with placeholder types, compile each missing formula field, and
/// repeat until nothing new resolves (formulas may reference each other).
pub fn infer_field_data_types(
    dataset: &dl_core::Dataset,
    dialect: Dialect,
) -> (
    std::collections::HashMap<String, dl_core::UserDataType>,
    Option<QueryError>,
) {
    use std::collections::HashMap;
    let mut first_formula_error: Option<QueryError> = None;
    let mut inferred: HashMap<String, dl_core::UserDataType> = HashMap::new();
    for _ in 0..4 {
        let mut patched = dataset.clone();
        for field in &mut patched.fields {
            if field.dtype().is_ok() {
                continue;
            }
            let placeholder = inferred
                .get(&field.guid)
                .copied()
                .unwrap_or(dl_core::UserDataType::Float);
            field.data_type = Some(placeholder);
            field.initial_data_type = Some(placeholder);
        }
        let spec = QuerySpec::default();
        let Ok(mut resolver) = Resolver::new(&patched, &spec, dialect) else {
            break;
        };
        let mut progressed = false;
        for field in &dataset.fields {
            if field.dtype().is_ok() || field.calc_mode != CalcMode::Formula {
                continue;
            }
            if inferred.contains_key(&field.guid) {
                continue;
            }
            match resolver.field_data_type(field) {
                Ok(data_type) => {
                    inferred.insert(
                        field.guid.clone(),
                        dl_core::UserDataType::from_data_type(data_type),
                    );
                    progressed = true;
                }
                // A formula that cannot be compiled (unknown field/function, bad syntax): the
                // reference reports the formula error, not a generic "field has no data type".
                Err(error @ QueryError::UnknownFieldInFormula(_)) if first_formula_error.is_none() => {
                    first_formula_error = Some(error);
                }
                Err(QueryError::Formula { guid, reason }) if first_formula_error.is_none() => {
                    let reason = unknown_function_reason(field.formula_text(), &reason)
                        .unwrap_or(reason);
                    first_formula_error = Some(QueryError::Formula { guid, reason });
                }
                Err(_) => {}
            }
        }
        if !progressed {
            break;
        }
    }
    (inferred, first_formula_error)
}

/// `dl_formula`'s `UnknownFunctionError` message, counted from the *original* formula text.
///
/// The translator's error carries the rendered call (`no translation for MOD(INTEGER|lit:int:2)
/// in postgres`), where the arguments have already been folded; the reference reports the arity
/// of the formula the user wrote (`Unknown 2-argument function or operator MOD`). Find the call
/// in the field's formula and count its top-level commas.
fn unknown_function_reason(formula: &str, reason: &str) -> Option<String> {
    let name = reason
        .split("no translation for ")
        .nth(1)?
        .split('(')
        .next()?
        .trim();
    if name.is_empty() {
        return None;
    }
    let lower = formula.to_ascii_lowercase();
    let needle = format!("{}(", name.to_ascii_lowercase());
    let start = lower.find(&needle)? + needle.len();
    let mut depth = 1usize;
    let mut commas = 0usize;
    let mut seen = false;
    for character in formula[start..].chars() {
        match character {
            '(' => {
                depth += 1;
                seen = true;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => commas += 1,
            value if !value.is_whitespace() => seen = true,
            _ => {}
        }
    }
    let arguments = if seen { commas + 1 } else { 0 };
    Some(format!("Unknown {arguments}-argument function or operator {name}"))
}

/// `DatasetValidator`'s formula pass: compile every formula field the way a query would.
///
/// `/result-preflight` reports these for all fields at once; `/result` would stop at the first
/// one, because `Resolver::expression` fails when it reaches it. `(guid, error)` per field, in
/// dataset order.
pub fn check_formulas(dataset: &Dataset, dialect: Dialect) -> Vec<(String, QueryError)> {
    let spec = QuerySpec::default();
    let Ok(mut resolver) = Resolver::new(dataset, &spec, dialect) else {
        // A dataset with a field of no type cannot even build a resolver; the structural
        // validation reports that field, so there is nothing to add here.
        return Vec::new();
    };
    dataset
        .fields
        .iter()
        .filter(|field| {
            field.calc_mode == CalcMode::Formula && !field.formula_text().trim().is_empty()
        })
        .filter_map(|field| {
            resolver
                .expression(field)
                .err()
                .map(|error| (field.guid.clone(), error))
        })
        .collect()
}

/// `FormulaCompiler.get_field_type` for every field of the dataset: `DIMENSION` / `MEASURE`.
///
/// Python sets `BIField.type` once per field, at the `aggregation` stage
/// (`formula_compiler.py:972-973`): the formula is complete by then — references substituted,
/// the cast applied — and wrapped in the field's own aggregation, so the field is a measure
/// exactly when *that* expression contains an aggregate call (`is_aggregate_expression`). The
/// answer does not depend on the request, which is why this is not a `Resolver` of the query
/// being built: `[Sales] + 1` is a measure for `/values/distinct` too, where nothing aggregates.
///
/// A window call is a measure only through its arguments: `RANK([Sales])` is `RANK(SUM(sales))`,
/// so it is a measure, while `RANK([Order date])` stays a dimension.
/// `get_field_type` fallback when `Resolver::expression` cannot compile the formula: decide
/// from the formula itself. `CONCAT(...)` (no aggregate call, no aggregated reference) is a
/// `DIMENSION`; the request's own `type` is the UI's guess and must not win over the formula.
fn analyzed_field_type(dataset: &Dataset, field: &Field) -> Option<FieldType> {
    if field.calc_mode != CalcMode::Formula {
        return None;
    }
    formula_has_aggregate(dataset, field, 0).map(|aggregated| {
        if aggregated {
            FieldType::Measure
        } else {
            FieldType::Dimension
        }
    })
}

/// Whether a formula field is a measure, decided from the formula tree the reference way:
/// the expression contains an aggregate call, or references a field whose own expression does.
///
/// Python applies the dataset's field expressions before it inspects the tree
/// (`_apply_aggregation` / `_substitute_fields_in_formula`), so `[COUNTS]/2` is a measure when
/// `COUNTS` is `count(...)` even though the formula text has no aggregate call. A string
/// formula like the pivot's `abs-retention` is therefore a measure too — the pivot role check
/// accepts it, and the string/boolean short-cut must not override that.
///
/// `None` means "cannot decide": a window expression (measure-ness depends on its arguments)
/// or a depth/parse limit, and the caller falls back to the dataset's saved answer.
fn formula_has_aggregate(dataset: &Dataset, field: &Field, depth: usize) -> Option<bool> {
    if depth > 4 {
        return None;
    }
    if field.aggregation.is_aggregate() {
        return Some(true);
    }
    if field.calc_mode != CalcMode::Formula {
        return Some(false);
    }
    let text = field.formula_text();
    if text.trim().is_empty() {
        return Some(false);
    }
    let item = dfl_native::parse_formula(text).ok()?;
    let analysis = dfl_native::analyze(&item);
    if !analysis.aggregates.is_empty() {
        return Some(true);
    }
    for name in &analysis.fields {
        let Some(reference) = dataset
            .fields
            .iter()
            .find(|candidate| candidate.title == *name || candidate.guid == *name)
        else {
            continue;
        };
        if formula_has_aggregate(dataset, reference, depth + 1) == Some(true) {
            return Some(true);
        }
    }
    if !analysis.windows.is_empty() {
        return None;
    }
    Some(false)
}

pub fn field_types(dataset: &Dataset, dialect: Dialect) -> HashMap<String, FieldType> {
    // `_apply_aggregation` is not gated on the query type: the dataset's own aggregation always
    // applies, which is what makes `[Sales] + 1` a measure here too.
    let spec = QuerySpec::default();
    let Ok(mut resolver) = Resolver::new(dataset, &spec, dialect) else {
        return HashMap::new();
    };
    dataset
        .fields
        .iter()
        .map(|field| {
            let field_type = match resolver.expression(field) {
                Ok(expression) if expression.aggregate_expression => FieldType::Measure,
                // The saved `type` is Python's own answer for this dataset, and it knows one
                // thing this pass cannot see: a direct column that the *source* pre-aggregates
                // (`has_auto_aggregation`), which also makes the field a measure. A string or
                // boolean formula, however, can never be a measure (`CONCAT(...)` is a
                // dimension even when the UI sent `type: MEASURE`).
                Ok(_) => analyzed_field_type(dataset, field).unwrap_or_else(|| dataset.field_type(field)),
                // `get_field_type` answers `_field_types.get(guid, DIMENSION)`: a field the
                // compiler never got to is a dimension, and the saved schema is the next best
                // evidence.
                Err(_) => analyzed_field_type(dataset, field).unwrap_or_else(|| dataset.field_type(field)),
            };
            (field.guid.clone(), field_type)
        })
        .collect()
}

/// Both field maps in one resolver pass.
///
/// The legend needs `DIMENSION`/`MEASURE` and `data_type` for every field. Computing them with
/// [`field_types`] and [`field_data_types`] separately creates two resolvers and translates every
/// formula twice; `Resolver::expression` already carries both answers.
pub fn field_types_and_data_types(
    dataset: &Dataset,
    dialect: Dialect,
) -> (HashMap<String, FieldType>, HashMap<String, UserDataType>) {
    let spec = QuerySpec::default();
    let Ok(mut resolver) = Resolver::new(dataset, &spec, dialect) else {
        return (HashMap::new(), HashMap::new());
    };
    let mut field_types = HashMap::with_capacity(dataset.fields.len());
    let mut data_types = HashMap::with_capacity(dataset.fields.len());
    for field in &dataset.fields {
        match resolver.expression(field) {
            Ok(expression) => {
                let field_type = if expression.aggregate_expression {
                    FieldType::Measure
                } else {
                    dataset.field_type(field)
                };
                field_types.insert(field.guid.clone(), field_type);
                data_types.insert(
                    field.guid.clone(),
                    UserDataType::from_data_type(expression.data_type),
                );
            }
            Err(error) => {
                field_types.insert(
                    field.guid.clone(),
                    analyzed_field_type(dataset, field).unwrap_or_else(|| dataset.field_type(field)),
                );
                let data_type = match error {
                    // The same fallbacks as `field_data_types`.
                    QueryError::Core(
                        CoreError::UnknownColumn { .. } | CoreError::UnknownSource { .. },
                    )
                    | QueryError::MissingSource { .. } => UserDataType::String,
                    _ => field
                        .dtype()
                        .map(UserDataType::from_data_type)
                        .unwrap_or(UserDataType::String),
                };
                data_types.insert(field.guid.clone(), data_type);
            }
        }
    }
    (field_types, data_types)
}

/// `FormulaCompiler.get_field_final_data_type` for every field: the legend's `data_type`.
///
/// Python recomputes a field's type from its formula on every validating pass
/// (`DatasetValidator._get_autoupdated_field`) and stores it back into `BIField.data_type`;
/// the legend then reports that value. The type is the one of the *prepared* expression —
/// cast applied, aggregation applied — and a field whose compilation fails keeps the stage
/// manager's fallback, `DEFAULT_DATA_TYPE` (`string`). That fallback is visible in the demo:
/// `Продажа, ₽` reads a `Price` column the `orders` table does not have, and Python reports
/// `data_type: "string"` for it.
pub fn field_data_types(dataset: &Dataset, dialect: Dialect) -> HashMap<String, UserDataType> {
    let spec = QuerySpec::default();
    let Ok(mut resolver) = Resolver::new(dataset, &spec, dialect) else {
        return HashMap::new();
    };
    dataset
        .fields
        .iter()
        .map(|field| {
            let data_type = match resolver.field_data_type(field) {
                Ok(data_type) => UserDataType::from_data_type(data_type),
                // A field the request cannot read at all — a stale avatar, a missing column —
                // is what Python falls back to `DEFAULT_DATA_TYPE` for. A formula the *query*
                // layer cannot translate yet (a lookup, a `TOTAL`) is a gap of this port, not
                // a broken field, so the declared type stays the best answer for it.
                Err(QueryError::Core(
                    CoreError::UnknownColumn { .. } | CoreError::UnknownSource { .. },
                ))
                | Err(QueryError::MissingSource { .. }) => UserDataType::String,
                Err(_) => field
                    .dtype()
                    .map(UserDataType::from_data_type)
                    .unwrap_or(UserDataType::String),
            };
            (field.guid.clone(), data_type)
        })
        .collect()
}

/// `_process_field_stage_casting`'s output: the prepared formula and the type of that formula.
///
/// The type is computed from the tree (`dfl_native::infer_type`), so a formula that the query
/// layer has to lift into a sub-query still has one.
struct PreparedFormula {
    item: Item,
    substitution: Substitution,
    analysis: dfl_native::Analysis,
    data_type: DataType,
}

/// One field's SQL at one of `dl_query_processing`'s processing stages.
/// Python has three stages (`initial` -> `formula_analysis` -> `casting` -> `aggregation` ->
/// `window`); the port needs the first two: [`Resolver::casting`] and [`Resolver::expression`].
#[derive(Debug, Clone)]
struct FieldExpr {
    sql: String,
    data_type: DataType,
    /// The expression carries an aggregation, so it belongs in `HAVING`, not in `WHERE`, and it
    /// may not be part of `GROUP BY`. A window call counts here: it cannot be grouped either.
    aggregated: bool,
    /// `dl_formula.inspect.expression.is_aggregate_expression()`: the expression contains an
    /// aggregate *call* — and a window call counts only through its arguments (`RANK(SUM(x))`
    /// is a measure, `RANK([dimension])` is not). This is what decides `DIMENSION`/`MEASURE`.
    aggregate_expression: bool,
    /// `dl_formula.inspect.expression.is_constant_expression()`: no column is referenced.
    is_constant: bool,
    /// The expression contains a window call (`is_window_expression`). A window cannot be part
    /// of `WHERE` or `HAVING` — it is only there once the rows are grouped — so a filter on it
    /// has to be applied to a sub-query that selects the window as a column.
    window: bool,
}

/// What `_substitute_fields_in_formula` learned while walking a formula's references.
#[derive(Debug, Default)]
struct Substitution {
    names: dfl_native::FieldNames,
    /// `[reference]` count of the formula, resolved or not.
    references: usize,
    /// Every reference resolved to a constant expression.
    all_constant: bool,
    /// A substituted expression carries an aggregation, so the formula is a measure even when
    /// its own text has no aggregate call (`[Orders]` -> `countd(t0.order_id)`).
    aggregated: bool,
    /// A substituted expression contains a window call, so the formula does too.
    window: bool,
    /// The unique names of the references whose expression aggregates (`sum(storage_fee)`,
    /// `(sum(a) - sum(b)) + ...`). `collapse_redundant_aggregates` needs them because the AST
    /// keeps a `Field` placeholder after substitution, not the aggregate call itself.
    aggregated_names: HashSet<String>,
}

struct Resolver<'a> {
    dataset: &'a Dataset,
    /// Values of the request: they override each parameter's `default_value`.
    parameter_values: &'a [ParameterValueSpec],
    /// The request itself: its dimensions are what `AMONG` is relative to, and its `ORDER BY`
    /// is the default ordering of a window function.
    spec: &'a QuerySpec,
    dialect: Dialect,
    /// `alias` of every `FROM` item, keyed by the table it reads.
    aliases: Vec<SourceAlias>,
    types: dfl_native::FieldTypes,
    used_sources: HashSet<String>,
    /// The root avatar: what a query with no direct field reads from, and where the `FROM`
    /// clause starts.
    root_source: Option<dl_core::Source>,
    /// Per-field results, so a field referenced by several formulas is compiled once.
    cache: HashMap<String, FieldExpr>,
    /// Formulas currently being compiled: a cycle in the references would not terminate.
    in_progress: HashSet<String>,
    /// Counter of the synthetic names substituted references are registered under.
    unique_names: usize,
    /// Leaves of every formula compiled so far, in the plan's numbering. A LOD leaf is what
    /// [`crate::fork::split`] turns into a sub-query; the rest stay in the base query.
    leaves: Vec<dfl_native::Leaf>,
    /// The request the windows of its formulas belong to: compiled once, on first use.
    window: Option<dfl_native::WindowContext>,
}

/// The table alias of one `FROM` item, keyed by the table it reads.
///
/// Python aliases a `FROM` item by its **avatar id** (`make_joined_from_for_avatars`). The port
/// numbers them instead (`t0`, `t1`, ...), which keeps the SQL readable; what matters is that the
/// alias is stable for the whole query, because a field of any avatar may be selected, filtered
/// or referenced by a window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceAlias {
    /// The avatar the item was created for; two avatars over one table share the alias only when
    /// they are not related (see [`source_aliases`]).
    id: String,
    table: String,
    subselect: Option<String>,
    alias: String,
}

impl SourceAlias {
    fn matches(&self, source: &dl_core::Source) -> bool {
        self.table == source.table && self.subselect == source.subselect
    }
}

/// Whether a relation joins the two avatars to each other.
///
/// Only a relation *between them* needs them to be two `FROM` items: a self-join over one table.
/// Two avatars over one table that no relation joins are the same table twice, and nothing to
/// join them with.
fn related_to(dataset: &Dataset, left: &str, right: &str) -> bool {
    dataset.avatar_relations.iter().any(|relation| {
        (relation.left_avatar_id == left && relation.right_avatar_id == right)
            || (relation.left_avatar_id == right && relation.right_avatar_id == left)
    })
}

/// One alias per `FROM` item of `dataset`, the root avatar's first.
///
/// Python aliases a `FROM` item by its **avatar id** (`make_joined_from_for_avatars`), so a
/// self-join over one table is two items with two aliases. The port numbers them instead (`t0`,
/// `t1`, ...), which keeps the SQL readable; what matters is that an alias is stable for the
/// whole query, because a field of any avatar may be selected, filtered or referenced by a
/// window. Avatars that no relation tells apart — the same table twice — share one alias, as they
/// shared one `FROM` item before the port knew about relations.
fn source_aliases(dataset: &Dataset) -> Vec<SourceAlias> {
    let mut ordered: Vec<&dl_core::Source> = Vec::with_capacity(dataset.sources.len());
    if let Some(root) = dataset.root_source() {
        ordered.push(root);
    }
    for source in &dataset.sources {
        if !ordered.iter().any(|other| other.id == source.id) {
            ordered.push(source);
        }
    }
    let mut aliases: Vec<SourceAlias> = Vec::with_capacity(ordered.len());
    for source in ordered {
        let shares = aliases
            .iter()
            .find(|alias| alias.matches(source) && !related_to(dataset, &alias.id, &source.id));
        if shares.is_some() {
            continue;
        }
        aliases.push(SourceAlias {
            id: source.id.clone(),
            table: source.table.clone(),
            subselect: source.subselect.clone(),
            alias: format!("t{}", aliases.len()),
        });
    }
    aliases
}

impl<'a> Resolver<'a> {
    fn new(
        dataset: &'a Dataset,
        spec: &'a QuerySpec,
        dialect: Dialect,
    ) -> Result<Self, QueryError> {
        let types = dataset.formula_field_types()?;
        let root_source = dataset.root_source().cloned();
        Ok(Self {
            dataset,
            parameter_values: &spec.parameter_values,
            spec,
            dialect,
            aliases: source_aliases(dataset),
            types,
            used_sources: HashSet::new(),
            root_source,
            cache: HashMap::new(),
            in_progress: HashSet::new(),
            unique_names: 0,
            leaves: Vec::new(),
            window: None,
        })
    }

    /// The alias of a source's `FROM` item.
    fn alias_for(&self, source: &dl_core::Source) -> &str {
        // The avatar's own item first: two avatars over one table are two aliases, and a
        // self-join's columns must name the right one.
        self.aliases
            .iter()
            .find(|alias| alias.id == source.id)
            .or_else(|| self.aliases.iter().find(|alias| alias.matches(source)))
            .map(|alias| alias.alias.as_str())
            .unwrap_or(FIRST_ALIAS)
    }

    /// The SQL of one field as a `SELECT`/`GROUP BY` item: everything the field's own
    /// aggregation applies to, applied.
    ///
    /// `FormulaCompiler._process_field_stage_aggregation`: a dataset field carries its own
    /// aggregation, so `[Sales]` (`sum`) is `SUM(t0."Sales")`, and a formula that *references*
    /// such a field inherits it (`[Delivered] / [Orders]` becomes a ratio of counts).
    fn expression(&mut self, field: &Field) -> Result<FieldExpr, QueryError> {
        let casting = self.casting(field)?;
        // `_apply_aggregation` is not gated on the query type: the dataset's own aggregation
        // applies to every field a query selects, preview included. A `GROUP BY`-less query with
        // an aggregated field would not even parse, which is why the preview groups by its
        // dimensions (`QuerySpec::group_by_if_measures`).
        //
        // A formula that already aggregates keeps its expression as it is: `[Sales] + 1` under
        // `aggregation: sum` is `SUM(t0."Sales") + 1`, not `SUM(SUM(...) + 1)` — the latter is
        // not valid SQL. Python reaches the same result through `has_auto_aggregation`, which it
        // computes from the very same check.
        if !field.aggregation.is_aggregate() || casting.aggregate_expression {
            return Ok(self.as_leaf(casting, field.calc_mode == CalcMode::Direct));
        }
        let FieldExpr { sql, data_type, .. } = casting;
        let aggregated = FieldExpr {
            sql: aggregate_sql(sql, field.aggregation),
            // The row decoder keeps reading the field's declared type, exactly what
            // `data-api` did before the aggregation moved in here.
            data_type,
            // `_apply_aggregation` wraps the whole expression, so the call is at the top.
            aggregated: true,
            aggregate_expression: true,
            is_constant: false,
            // `SUM(col)` is not a window, whatever the column's own formula was.
            window: false,
        };
        Ok(self.as_leaf(aggregated, field.calc_mode == CalcMode::Direct))
    }

    /// Put a direct field's expression into the plan as a leaf.
    ///
    /// A formula compiles into leaves itself ([`dfl_native::compile_leaves_from`]), but a
    /// `direct` field is a column the compiler renders straight (`t0."Region"` or
    /// `SUM(t0."Sales")`). A fork needs that column to be a column of the base query, so it
    /// has to be a leaf like any other. This is the identity for a non-direct field and for a
    /// request without forks, where [`crate::fork::split`] inlines the leaves back.
    fn as_leaf(&mut self, expression: FieldExpr, lift: bool) -> FieldExpr {
        if !lift {
            return expression;
        }
        // The same column is often selected twice (`[Region]` and a formula over it): one
        // column, one `e_N`.
        if let Some(index) = self
            .leaves
            .iter()
            .position(|leaf| leaf.sql == expression.sql && leaf.external.is_none())
        {
            return FieldExpr {
                sql: format!("{{leaf:{index}}}"),
                ..expression
            };
        }
        let index = self.leaves.len();
        self.leaves.push(dfl_native::Leaf {
            sql: expression.sql.clone(),
            data_type: expression.data_type.native(),
            aggregated: expression.aggregated,
            external: None,
            // A direct field is a column: even when its formula is a window, the leaf is the
            // window itself, and a window is computed where the rows are grouped.
            window: expression.window,
        });
        FieldExpr {
            sql: format!("{{leaf:{index}}}"),
            ..expression
        }
    }

    /// Fold the leaves a dimension's formula lifted back into one leaf.
    ///
    /// A dimension is a column of the base query: `CAST(DATE_PART('year', [date]) AS BIGINT)`
    /// has to be selected and grouped by as one expression. Left as a template over the raw
    /// `date`, the base query would select `t0."date"` but group by the cast, and PostgreSQL
    /// rejects that ("column ... must appear in the GROUP BY clause"). A LOD inside the formula
    /// is a fork of its own and is never folded.
    fn collapse(&mut self, expression: FieldExpr) -> FieldExpr {
        if !expression.sql.contains("{leaf:") {
            return expression;
        }
        // A single leaf is already one expression: `COUNTD(...)` is a column of `q_0`.
        let single = expression
            .sql
            .strip_prefix("{leaf:")
            .and_then(|rest| rest.strip_suffix('}'))
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()));
        if single {
            return expression;
        }
        let mut sql = expression.sql.clone();
        loop {
            let mut replaced = false;
            for (index, leaf) in self.leaves.iter().enumerate() {
                if leaf.external.is_some() {
                    // A fork cannot be inlined back: it is a query of its own.
                    return expression;
                }
                let token = format!("{{leaf:{index}}}");
                if sql.contains(&token) {
                    sql = sql.replace(&token, &leaf.sql);
                    replaced = true;
                }
            }
            if !replaced {
                break;
            }
        }
        if let Some(index) = self
            .leaves
            .iter()
            .position(|leaf| leaf.sql == sql && leaf.external.is_none())
        {
            return FieldExpr {
                sql: format!("{{leaf:{index}}}"),
                ..expression
            };
        }
        let index = self.leaves.len();
        self.leaves.push(dfl_native::Leaf {
            sql,
            data_type: expression.data_type.native(),
            aggregated: expression.aggregated,
            external: None,
            // A collapsed dimension is one expression, and a window in it stays in it: no
            // dimension formula is a window of the request's own.
            window: expression.window,
        });
        FieldExpr {
            sql: format!("{{leaf:{index}}}"),
            ..expression
        }
    }

    /// The SQL of one field *before* its own aggregation (`ProcessingStage.casting`).
    ///
    /// This is what an aggregated reference inside a formula is replaced with
    /// (`_suppress_double_aggregations`): `SUM([Sales])` must stay `SUM(t0."Sales")`, not become
    /// `SUM(SUM(t0."Sales"))`.
    fn casting(&mut self, field: &Field) -> Result<FieldExpr, QueryError> {
        if let Some(cached) = self.cache.get(&field.guid) {
            return Ok(cached.clone());
        }
        let result = self.casting_uncached(field)?;
        self.cache.insert(field.guid.clone(), result.clone());
        Ok(result)
    }

    fn casting_uncached(&mut self, field: &Field) -> Result<FieldExpr, QueryError> {
        match field.calc_mode {
            // A parameter is a constant: the request value, else the default.
            CalcMode::Parameter => {
                let data_type = field.dtype()?;
                let value =
                    self.parameter_value(field)
                        .ok_or_else(|| QueryError::MissingParameter {
                            guid: field.guid.clone(),
                        })?;
                Ok(FieldExpr {
                    sql: self.dialect.render_literal(&value, data_type)?,
                    data_type,
                    aggregated: false,
                    aggregate_expression: false,
                    is_constant: true,
                    window: false,
                })
            }
            CalcMode::Direct => {
                let source =
                    self.dataset
                        .source_for(field)
                        .ok_or_else(|| QueryError::MissingSource {
                            guid: field.guid.clone(),
                        })?;
                self.used_sources.insert(source.id.clone());
                let column = field.physical_column();
                // A `direct` field's type is the one the dataset declares (`field.data_type`,
                // which `DatasetValidator` keeps up to date): Python's casting stage compares
                // against it, so a `string` column read by an `integer` field is cast by that
                // field and not the other way round. The schema is consulted for existence
                // only — a column that is not there is a broken field, which is what Python
                // resolves through `DataSource.raw_schema`.
                if !source.columns.is_empty() && source.column(column).is_none() {
                    return Err(CoreError::UnknownColumn {
                        guid: field.guid.clone(),
                        column: column.to_string(),
                        source_id: source.id.clone(),
                    }
                    .into());
                }
                Ok(FieldExpr {
                    sql: format!(
                        "{}.{}",
                        self.alias_for(source),
                        self.dialect.quote_ident(column)
                    ),
                    data_type: field.dtype()?,
                    aggregated: false,
                    // A plain column is not an aggregate; a pre-aggregated one is invisible
                    // here, which is why `field_types` keeps the stored `MEASURE`.
                    aggregate_expression: false,
                    is_constant: false,
                    window: false,
                })
            }
            CalcMode::Formula => self.formula(field),
        }
    }

    /// A formula field with parameters substituted and every reference replaced by the SQL of the
    /// field it names, compiled by `dfl_native`.
    ///
    /// `_substitute_fields_in_formula` walks the *tree*, so a reference may be replaced by any
    /// node — a column, a cast, an aggregate call. That is what `FieldName::Sql` is for.
    fn formula(&mut self, field: &Field) -> Result<FieldExpr, QueryError> {
        if !self.in_progress.insert(field.guid.clone()) {
            return Err(QueryError::Formula {
                guid: field.guid.clone(),
                reason: "the formula references itself".to_string(),
            });
        }
        let result = self.compile_formula_field(field);
        self.in_progress.remove(&field.guid);
        result
    }

    fn compile_formula_field(&mut self, field: &Field) -> Result<FieldExpr, QueryError> {
        let PreparedFormula {
            item,
            substitution,
            analysis,
            data_type,
        } = self.prepare_formula(field)?;
        let dialect = self.dfl_dialect()?;
        // A formula with a window call needs the request around it: the dimensions `AMONG` is
        // relative to and the `ORDER BY` a window function orders by default.
        let window = if analysis.has_window {
            self.window_context()?
        } else {
            dfl_native::WindowContext::default()
        };
        // Leaf mode, not `compile_named`: a formula with a LOD clause is not an expression,
        // it is a template over the columns a plan selects. Outside a request with forks the
        // leaves are inlined back, so the SQL stays the one `compile_named` produced.
        let compiled = dfl_native::compile_leaves_from(
            &item,
            dialect,
            &self.types,
            &substitution.names,
            self.leaves.len(),
            &window,
        )
        .map_err(|reason| QueryError::Formula {
            guid: field.guid.clone(),
            reason,
        })?;
        self.leaves.extend(compiled.leaves.iter().cloned());
        // A formula with no aggregate call of its own can still be a measure: `[Orders]` *is*
        // `countd(t0.order_id)` once its reference is substituted.
        let is_constant = if substitution.references == 0 {
            analysis.is_constant
        } else {
            substitution.all_constant
        };
        Ok(FieldExpr {
            sql: format!("({})", compiled.sql),
            data_type,
            window: analysis.has_window || substitution.window,
            aggregated: analysis.has_aggregate || analysis.has_window || substitution.aggregated,
            // `is_aggregate_expression` does not look inside a window call for the call itself:
            // `RANK(SUM(x))` is a measure because of the `SUM` in its arguments, and
            // `RANK([dimension])` is not a measure at all.
            aggregate_expression: analysis.has_aggregate || substitution.aggregated,
            is_constant,
        })
    }

    /// What the request gives every window function in it (`FormulaCompiler.apply_mutations`).
    ///
    /// * `AMONG [d]` is `WITHIN <the request's dimensions except d>`, so the dimensions have to
    ///   be rendered the way a `[reference]` to them would be — as the plan's leaf when the
    ///   field is direct, so that the clause and the reference to the same field match.
    /// * A window function without an ordering of its own takes the request's `ORDER BY`
    ///   (`DefaultWindowOrderingMutation`); without it `RSUM` sums the whole partition.
    ///
    /// Computed once per request, and only for a formula that has a window: rendering a
    /// dimension may compile another field, which is wasted work (and a recursion) otherwise.
    fn window_context(&mut self) -> Result<dfl_native::WindowContext, QueryError> {
        if let Some(cached) = &self.window {
            return Ok(cached.clone());
        }
        let spec = self.spec;
        let dataset = self.dataset;
        let mut context = dfl_native::WindowContext::default();
        let dimensions: Vec<dl_core::FieldRef> = spec
            .select
            .iter()
            .filter(|item| item.role.is_selectable())
            .map(|item| item.field_ref.clone())
            .collect();
        for field_ref in dimensions {
            let Some(field) = dataset.resolve(&field_ref) else {
                continue;
            };
            if field.calc_mode == CalcMode::Formula && self.in_progress.contains(&field.guid) {
                // The formula being compiled is a dimension of the request itself: its own
                // expression is what is being built right now.
                continue;
            }
            if dataset.field_type(field) != dl_core::FieldType::Dimension {
                continue;
            }
            let expression = self.expression(field)?;
            if expression.aggregated || expression.is_constant {
                continue;
            }
            // `AMONG [d]` names a dimension the same way a `[d]` reference does: as the leaf the
            // plan selects, so that the clause and the reference to the dimension are the same
            // expression (and a fork can name it as a column).
            let expression = self.collapse(expression);
            context.group_dims.push(expression.sql);
        }
        for order in &spec.order_by {
            let Some(field) = dataset.resolve(&order.field_ref) else {
                continue;
            };
            if field.calc_mode == CalcMode::Formula && self.in_progress.contains(&field.guid) {
                continue;
            }
            let expression = self.expression(field)?;
            context
                .ordering
                .push((expression.sql, order.direction == Direction::Desc));
        }
        self.window = Some(context.clone());
        Ok(context)
    }

    fn dfl_dialect(&self) -> Result<dfl_native::Dialect, QueryError> {
        dfl_native::Dialect::parse(self.dialect.as_str())
            .ok_or_else(|| QueryError::Invalid(self.dialect.as_str().to_string()))
    }

    /// The type of a field after substitution and cast (`get_field_final_data_type`), without
    /// rendering its SQL.
    fn field_data_type(&mut self, field: &Field) -> Result<DataType, QueryError> {
        match field.calc_mode {
            CalcMode::Parameter => Ok(field.dtype()?),
            CalcMode::Direct => {
                let source =
                    self.dataset
                        .source_for(field)
                        .ok_or_else(|| QueryError::MissingSource {
                            guid: field.guid.clone(),
                        })?;
                let column = field.physical_column();
                if !source.columns.is_empty() && source.column(column).is_none() {
                    return Err(CoreError::UnknownColumn {
                        guid: field.guid.clone(),
                        column: column.to_string(),
                        source_id: source.id.clone(),
                    }
                    .into());
                }
                Ok(field.dtype()?)
            }
            CalcMode::Formula => {
                if !self.in_progress.insert(field.guid.clone()) {
                    return Err(QueryError::Formula {
                        guid: field.guid.clone(),
                        reason: "the formula references itself".to_string(),
                    });
                }
                let result = self
                    .prepare_formula(field)
                    .map(|prepared| prepared.data_type);
                self.in_progress.remove(&field.guid);
                result
            }
        }
    }

    /// A field's formula with the references substituted and the dataset's `cast` applied —
    /// `_process_field_stage_substitution` followed by `_process_field_stage_casting`.
    ///
    /// The type comes from [`dfl_native::infer_type`], not from the rendered SQL: a formula
    /// the query layer has to lift into a sub-query (`SUM([x] TOTAL)`, a lookup) still has a
    /// type, and Python reports it in the legend while the translation stays unsupported.
    fn prepare_formula(&mut self, field: &Field) -> Result<PreparedFormula, QueryError> {
        let text = self.substitute_parameters(field.formula_text());
        let mut item = dfl_native::parse_formula(&text).map_err(|reason| QueryError::Formula {
            guid: field.guid.clone(),
            reason,
        })?;
        let analysis = dfl_native::analyze(&item);
        let substitution = self.substitute_fields(&mut item)?;
        // `SUM([aggregated field])` would nest aggregates; drop the identity outer call before
        // the tree is rendered (see `collapse_redundant_aggregates`).
        collapse_redundant_aggregates(&mut item, &substitution.aggregated_names);
        let dialect = self.dfl_dialect()?;
        let infer = |item: &Item| {
            dfl_native::infer_type(item, dialect, &self.types).map_err(|reason| {
                QueryError::Formula {
                    guid: field.guid.clone(),
                    reason,
                }
            })
        };
        let mut data_type = DataType::from_native(infer(&item)?);
        // `_process_field_stage_casting`: the dataset's `cast` is applied to the *prepared*
        // formula, so the type is known only after the references are substituted. A field
        // declared `date` whose formula produces a timestamp (`date_trunc(...)`) is therefore
        // `CAST(... AS DATE)` — without that the row decoder sees a datetime, and the pivot
        // header the frontend sends back says `2020-02-01T00:00:00` instead of `2020-02-01`.
        if let Some(function) = cast_function(data_type, field.cast) {
            let casted = Item::Call {
                name: function.into_boxed_str(),
                args: vec![item],
                clauses: Box::new(dfl_native::ast::Clauses::default()),
            };
            data_type = DataType::from_native(infer(&casted)?);
            item = casted;
        }
        Ok(PreparedFormula {
            item,
            substitution,
            analysis,
            data_type,
        })
    }

    /// The dataset field a formula reference names: a guid in `guid_formula`, a title otherwise.
    fn reference(&self, name: &str) -> Option<&'a Field> {
        let dataset: &'a Dataset = self.dataset;
        dataset
            .field_by_guid(name)
            .or_else(|| dataset.field_by_title(name))
    }

    /// `_substitute_fields_in_formula`: replace every `[reference]` of `item` with the SQL of the
    /// field it names.
    ///
    /// A reference that sits inside an aggregate call keeps the field's expression *before* its
    /// own aggregation (`_suppress_double_aggregations`), every other one gets the field's full
    /// expression. `dl_formula` decides that per *occurrence*, so `[Sales] / SUM([Sales])` becomes
    /// `SUM(sales) / SUM(sales)`, not `SUM(SUM(sales))`.
    fn substitute_fields(&mut self, item: &mut Item) -> Result<Substitution, QueryError> {
        let mut result = Substitution {
            all_constant: true,
            ..Substitution::default()
        };
        self.substitute_item(item, false, &mut result)?;
        Ok(result)
    }

    /// The tree walk of [`Resolver::substitute_fields`].
    ///
    /// `aggregated_above` is `dl_formula.inspect.expression.is_aggregated_above_sub_node()`: an
    /// ancestor is an aggregate *function call* (`is_aggregate_function()`, so a window call does
    /// not count — `SUM(SUM(x) AMONG y)` is `SUM(x AMONG y)` over the aggregated `x`).
    fn substitute_item(
        &mut self,
        item: &mut Item,
        aggregated_above: bool,
        result: &mut Substitution,
    ) -> Result<(), QueryError> {
        match item {
            Item::Field(name) => {
                result.references += 1;
                let Some(child) = self.reference(name) else {
                    // A formula reference the dataset does not have is a hard error in
                    // `dl_formula` (`UnknownFieldInFormulaError`); rendering it as a column
                    // would push the failure into the database and report it as a translation
                    // error instead of naming the missing field.
                    return Err(QueryError::UnknownFieldInFormula(name.to_string()));
                };
                let expression = if aggregated_above {
                    self.casting(child)?
                } else {
                    // A reference outside an aggregate is the field's own expression — and the
                    // plan has to be able to *name* it: a fork puts the formula in an outer
                    // `SELECT`, where the source columns it is built from are no longer visible,
                    // so `[Order month]` has to be the column the dimension selects (`q_0.e_3`)
                    // and not the `date_trunc` over the raw date.
                    let referenced = self.expression(child)?;
                    self.collapse(referenced)
                };
                result.aggregated |= expression.aggregated;
                result.window |= expression.window;
                result.all_constant &= expression.is_constant;
                // Two occurrences of one name may need different substitutions, and `FieldNames`
                // is keyed by name, so every occurrence gets a name of its own. The name is never
                // rendered (the SQL replaces it) and never re-parsed, it only has to carry the
                // expression's data type through the type inference of the compiled formula.
                let unique = format!("@sub{}", self.unique_names);
                self.unique_names += 1;
                if expression.aggregated {
                    result.aggregated_names.insert(unique.clone());
                }
                self.types
                    .insert(unique.clone(), expression.data_type.native());
                result
                    .names
                    .insert(unique.clone(), FieldName::Sql(expression.sql));
                *name = unique.into_boxed_str();
                Ok(())
            }
            Item::Unary(_, child) => self.substitute_item(child, aggregated_above, result),
            Item::Binary(_, left, right) => {
                self.substitute_item(left, aggregated_above, result)?;
                self.substitute_item(right, aggregated_above, result)
            }
            Item::In { value, list, .. } => {
                self.substitute_item(value, aggregated_above, result)?;
                for element in list {
                    self.substitute_item(element, aggregated_above, result)?;
                }
                Ok(())
            }
            Item::Is { value, .. } => self.substitute_item(value, aggregated_above, result),
            Item::Between {
                value, low, high, ..
            } => {
                self.substitute_item(value, aggregated_above, result)?;
                self.substitute_item(low, aggregated_above, result)?;
                self.substitute_item(high, aggregated_above, result)
            }
            Item::Case {
                operand,
                branches,
                else_,
            } => {
                if let Some(operand) = operand {
                    self.substitute_item(operand, aggregated_above, result)?;
                }
                for (when, then) in branches {
                    self.substitute_item(when, aggregated_above, result)?;
                    self.substitute_item(then, aggregated_above, result)?;
                }
                if let Some(else_) = else_ {
                    self.substitute_item(else_, aggregated_above, result)?;
                }
                Ok(())
            }
            Item::Call {
                name,
                args,
                clauses,
            } => {
                let props = dfl_native::func_props::call_props(name);
                let window =
                    props.can_be_window && (!props.can_be_nonwindow || clauses.has_grouping());
                // A fork is computed over raw rows: `SUM([Sales] TOTAL)` becomes
                // `SELECT SUM(t0."amount")` in a sub-query of its own, so its argument is the
                // field *before* its own aggregation. A window that stays in the base query
                // (`RANK([Sales] WITHIN …)`) keeps `SUM(t0."amount")` as its argument, because
                // the base query has already grouped.
                // `INCLUDE` is the exception: it *lowers* the level of detail, so the field's
                // own aggregation happens at the new level — `AVG([Clients] INCLUDE [date])`
                // averages `COUNT(DISTINCT user)` per date, not `AVG(user_id)`.
                let forked = clauses
                    .lod
                    .as_ref()
                    .is_some_and(|lod| !matches!(lod.kind, dfl_native::LodKind::Include))
                    || clauses.grouping == dfl_native::Grouping::Total
                    || (clauses.grouping == dfl_native::Grouping::Within
                        && !clauses.bfb.is_empty());
                // `AVG([Sales] TOTAL)` aggregates the *grouped* sums: the field keeps its own
                // aggregation inside the call, and the fork turns into two queries
                // (`fork::nested_total_call`). Suppressing it here would average the raw
                // amounts instead — a different number, not a different plan.
                let total_over_aggregate = clauses.grouping == dfl_native::Grouping::Total
                    && !crate::fork::is_distributive_aggregate(name);
                let inside = if total_over_aggregate {
                    aggregated_above
                } else {
                    aggregated_above || forked || (props.can_be_aggregate && !window)
                };
                for arg in args {
                    self.substitute_item(arg, inside, result)?;
                }
                for dimension in &mut clauses.grouping_dims {
                    self.substitute_item(dimension, inside, result)?;
                }
                // The dimensions of a LOD clause are references too: `COUNTD([Sales] FIXED
                // [Region])` counts by `[Region]`, and the query plan joins the fork on the
                // SQL of that dimension. Python's `_substitute_fields_in_formula` walks
                // `node.system_fields`, which is where `dl_formula` keeps them.
                if let Some(lod) = &mut clauses.lod {
                    for dimension in &mut lod.dims {
                        self.substitute_item(dimension, false, result)?;
                    }
                }
                for (key, _) in &mut clauses.ordering {
                    self.substitute_item(key, inside, result)?;
                }
                Ok(())
            }
            // Literals, `#date#`s and `NULL` hold no reference.
            _ => Ok(()),
        }
    }

    /// `parameter_values` entry for `field`, else its `default_value`.
    fn parameter_value(&self, field: &Field) -> Option<serde_json::Value> {
        self.parameter_values
            .iter()
            .find(|value| match &value.field_ref {
                dl_core::FieldRef::Id { id } => id == &field.guid,
                dl_core::FieldRef::Title { title } => title == &field.title,
                dl_core::FieldRef::MeasureName
                | dl_core::FieldRef::DimensionName
                | dl_core::FieldRef::Placeholder => false,
            })
            .map(|value| value.value.clone())
            .or_else(|| field.default_value.clone())
    }

    /// The `FROM` clause body: the sources the request touches, joined by their avatar relations.
    ///
    /// A table source is quoted (`quote_table`). A custom-SQL source is not a table at all:
    /// `SubselectDataSource.get_sql_source` embeds the text verbatim as
    /// `(\n{subsql}\n) AS <alias>` — PostgreSQL rejects an unaliased subquery, which is why
    /// the alias is mandatory there.
    ///
    /// `build_from_avatar_join_info` walks the relations from the root avatar towards the avatars
    /// the query reads, so the clause is `root JOIN next ON (...) JOIN next2 ON (...)`. A dataset
    /// whose used avatars are not connected by relations cannot be answered: no SQL would read
    /// both tables, which is [`QueryError::MultipleSources`].
    fn from_item(&self) -> Result<String, QueryError> {
        // `collect_avatar_join_info_list` always starts from the root avatar, even when no field
        // of the request reads it: it is the left side of every join, and `make_joined_from_for_avatars`
        // puts it in the `FROM` unconditionally.
        let mut items: Vec<&dl_core::Source> = Vec::with_capacity(self.used_sources.len() + 1);
        if let Some(root) = &self.root_source {
            items.push(root);
        }
        for source in self
            .dataset
            .sources
            .iter()
            .filter(|source| self.used_sources.contains(&source.id))
        {
            // An avatar that shares its `FROM` item with one already there (same alias) adds
            // nothing: it is the same table read under the same name.
            if items
                .iter()
                .any(|item| self.alias_for(item) == self.alias_for(source))
            {
                continue;
            }
            items.push(source);
        }
        if items.is_empty() {
            // A dataset with no sources at all: nothing to read, nothing to join.
            return Ok(String::new());
        }
        let mut sql = self.render_source(items[0]);
        let mut joined: Vec<&dl_core::Source> = vec![items[0]];
        let mut pending: Vec<&dl_core::Source> = items[1..].to_vec();
        while !pending.is_empty() {
            let next = pending.iter().enumerate().find_map(|(index, candidate)| {
                self.dataset
                    .relation_to(&joined, candidate)
                    .map(|(relation, swapped)| (index, relation, swapped))
            });
            let Some((index, relation, swapped)) = next else {
                break;
            };
            let candidate = pending.remove(index);
            sql.push_str(&format!(
                " {} {} ON {}",
                relation.join_type.sql(),
                self.render_source(candidate),
                self.join_conditions(relation, swapped)?
            ));
            joined.push(candidate);
        }
        if !pending.is_empty() {
            return Err(QueryError::MultipleSources(items.len()));
        }
        Ok(sql)
    }

    /// One source as a `FROM` item, with the query alias.
    fn render_source(&self, source: &dl_core::Source) -> String {
        let alias = self.alias_for(source);
        match &source.subselect {
            Some(subsql) => format!("(\n{}\n) AS {}", subsql.trim(), alias),
            None => format!("{} AS {}", self.dialect.quote_table(&source.table), alias),
        }
    }

    /// The `ON` clause of one relation: its conditions, `AND`-ed together.
    ///
    /// A condition compares two columns, one of each avatar. `swapped` says the join is rendered
    /// with the relation's ends exchanged ([`Dataset::relation_to`]), so the sides swap too.
    fn join_conditions(
        &self,
        relation: &dl_core::AvatarRelation,
        swapped: bool,
    ) -> Result<String, QueryError> {
        let mut parts: Vec<String> = Vec::with_capacity(relation.conditions.len());
        for condition in &relation.conditions {
            let (left, right, left_part, right_part) = match swapped {
                true => (
                    &relation.right_avatar_id,
                    &relation.left_avatar_id,
                    &condition.right,
                    &condition.left,
                ),
                false => (
                    &relation.left_avatar_id,
                    &relation.right_avatar_id,
                    &condition.left,
                    &condition.right,
                ),
            };
            let operator = match condition.operator.trim().to_ascii_lowercase().as_str() {
                "eq" => "=",
                "ne" => "<>",
                "gt" => ">",
                "lt" => "<",
                "gte" => ">=",
                "lte" => "<=",
                other => {
                    return Err(QueryError::Invalid(format!(
                        "unsupported join condition operator: {other}"
                    )))
                }
            };
            parts.push(format!(
                "{} {operator} {}",
                self.join_part(left, left_part)?,
                self.join_part(right, right_part)?
            ));
        }
        Ok(parts.join(" AND "))
    }

    /// One side of a join condition: `<alias>."<column>"`.
    ///
    /// Only a `direct` part — a physical column of the avatar's own table — can be rendered; the
    /// other two spellings of `ConditionPart` are compiled by Python into a `BIField`, which the
    /// port cannot build outside a whole query, and the demo does not use them.
    fn join_part(
        &self,
        avatar_id: &str,
        part: &dl_core::JoinConditionPart,
    ) -> Result<String, QueryError> {
        let source = self
            .dataset
            .sources
            .iter()
            .find(|source| source.id == avatar_id)
            .ok_or_else(|| QueryError::MissingSource {
                guid: avatar_id.to_string(),
            })?;
        let column = part.column().ok_or_else(|| {
            QueryError::Invalid(format!(
                "the join condition of avatar {avatar_id} is not a direct column"
            ))
        })?;
        Ok(format!(
            "{}.{}",
            self.alias_for(source),
            self.dialect.quote_ident(column)
        ))
    }

    ///
    /// Python substitutes parameters inside the formula compiler; the port replaces the
    /// reference before parsing, which is the same thing for well-formed formulas.
    fn substitute_parameters(&self, text: &str) -> String {
        let mut result = text.to_string();
        for field in self
            .dataset
            .fields
            .iter()
            .filter(|field| field.is_parameter())
        {
            let (Some(value), Ok(dtype)) = (self.parameter_value(field), field.dtype()) else {
                continue;
            };
            let Some(literal) = parameter_formula_literal(&value, dtype) else {
                continue;
            };
            for token in [format!("[{}]", field.title), format!("[{}]", field.guid)] {
                result = result.replace(&token, &literal);
            }
        }
        result
    }
}

impl<'a> Resolver<'a> {
    /// `_apply_wrapper_array_prefix` / `_apply_func_wrapper`: put a formula call around an
    /// expression that is already SQL.
    ///
    /// `dl_formula` does exactly this in Python — it builds the node and lets the registry pick
    /// the template — so the port compiles the wrapper as a formula whose only reference is
    /// substituted with the SQL of the wrapped expression.
    fn wrapper(&self, formula: &str, expr: &str, dtype: DataType) -> Result<String, QueryError> {
        const WRAPPED: &str = "__wrapped";
        let mut names = dfl_native::FieldNames::new();
        names.insert(WRAPPED.to_string(), FieldName::Sql(expr.to_string()));
        let mut types = dfl_native::FieldTypes::new();
        types.insert(WRAPPED.to_string(), dtype.native());
        dfl_native::compile_formula_named(formula, self.dialect.as_str(), &types, &names)
            .map(|compiled| compiled.sql)
            .map_err(|reason| {
                QueryError::Invalid(format!("cannot build the {WRAPPED} wrapper: {reason}"))
            })
    }

    /// `ArrayPrefixSelectWrapperSpec(length=level)`: `slice(expr, 1, level)`.
    fn slice_wrapper(
        &self,
        expr: &str,
        dtype: DataType,
        level: Option<i64>,
    ) -> Result<String, QueryError> {
        // `TreeRoleSpec.level` is required by the API, so a request without one is malformed.
        let level = level.ok_or_else(|| {
            QueryError::Invalid("the tree role needs a level to slice the array with".to_string())
        })?;
        self.wrapper(&format!("SLICE([__wrapped], 1, {level})"), expr, dtype)
    }

    /// A `WHERE`/`HAVING` fragment for one request filter, and which clause it belongs to.
    ///
    /// `None` means the filter was dropped by `ignore_nonexistent_filters`.
    fn filter(
        &mut self,
        spec: &FilterSpec,
        ignore_unknown: bool,
    ) -> Result<Option<FilterPart>, QueryError> {
        let field = match self.dataset.resolve(&spec.field_ref) {
            Some(field) => field,
            None if ignore_unknown => return Ok(None),
            None => return Err(unknown(&spec.field_ref)),
        };
        let guid = field.guid.clone();
        // A filter compares the field the way `SELECT` renders it, aggregation included — and an
        // aggregation is not allowed in `WHERE`, which is why such a filter becomes `HAVING`.
        let FieldExpr {
            sql: expr,
            data_type: dtype,
            aggregated: is_having,
            window: is_window,
            ..
        } = self.expression(field)?;
        let values = &spec.values;

        let expect = |expected: &'static str, actual: usize, operation: &'static str| {
            if actual == 0 {
                Err(QueryError::BadValueCount {
                    guid: guid.clone(),
                    operation,
                    expected,
                    actual,
                })
            } else {
                Ok(())
            }
        };

        let condition: Result<String, QueryError> = match spec.operation {
            FilterOperation::Isnull => Ok(format!("{expr} IS NULL")),
            FilterOperation::Isnotnull => Ok(format!("{expr} IS NOT NULL")),
            FilterOperation::Eq
            | FilterOperation::Ne
            | FilterOperation::Gt
            | FilterOperation::Lt
            | FilterOperation::Gte
            | FilterOperation::Lte => {
                expect("exactly one value", values.len(), "a comparison")?;
                let literal = self.dialect.render_literal(&values[0], dtype)?;
                let operator = match spec.operation {
                    FilterOperation::Eq => "=",
                    FilterOperation::Ne => "!=",
                    FilterOperation::Gt => ">",
                    FilterOperation::Lt => "<",
                    FilterOperation::Gte => ">=",
                    _ => "<=",
                };
                Ok(format!("{expr} {operator} {literal}"))
            }
            FilterOperation::In | FilterOperation::Nin => {
                let negated = spec.operation == FilterOperation::Nin;
                if values.is_empty() {
                    // `IN ()` is false and `NOT IN ()` is true, exactly like `dl_formula`.
                    let constant = if negated { "TRUE" } else { "FALSE" };
                    return Ok(Some(FilterPart {
                        expr,
                        condition: constant.to_string(),
                        having: is_having,
                        window: is_window,
                        alias: self.dialect.quote_ident(&guid),
                    }));
                }
                // A `null` in the list is not a value to compare with: `dl_formula` renders
                // `in`/`notin` through `_in_fix_null`/`_not_in_fix_null`, which take the nulls
                // out of the list and add a separate `IS [NOT] NULL` branch instead. Without
                // that branch `x NOT IN (...)` silently drops the rows where `x` is NULL.
                let has_null = values.iter().any(|value| value.is_null());
                let literals = values
                    .iter()
                    .filter(|value| !value.is_null())
                    .map(|value| self.dialect.render_literal(value, dtype))
                    .collect::<Result<Vec<_>, _>>()?;
                let list = literals.join(", ");
                Ok(match (negated, has_null) {
                    (false, false) => format!("{expr} IN ({list})"),
                    (false, true) if list.is_empty() => format!("{expr} IS NULL"),
                    (false, true) => format!("({expr} IS NULL OR {expr} IN ({list}))"),
                    (true, true) if list.is_empty() => format!("{expr} IS NOT NULL"),
                    (true, true) => format!("({expr} NOT IN ({list}) AND {expr} IS NOT NULL)"),
                    (true, false) => format!("({expr} NOT IN ({list}) OR {expr} IS NULL)"),
                })
            }
            FilterOperation::Between => {
                if values.len() != 2 {
                    return Err(QueryError::BadValueCount {
                        guid: guid.clone(),
                        operation: "BETWEEN",
                        expected: "exactly two values",
                        actual: values.len(),
                    });
                }
                let low = self.dialect.render_literal(&values[0], dtype)?;
                let high = self.dialect.render_literal(&values[1], dtype)?;
                Ok(format!("{expr} BETWEEN {low} AND {high}"))
            }
            FilterOperation::Contains
            | FilterOperation::Icontains
            | FilterOperation::Notcontains
            | FilterOperation::Noticontains
            | FilterOperation::Startswith
            | FilterOperation::Istartswith
            | FilterOperation::Endswith
            | FilterOperation::Iendswith => {
                expect("exactly one value", values.len(), "a substring match")?;
                // `FuncStartswithArrayConst`: on a list-valued field a `tree` level is locked by
                // comparing its leading entries with the prefix itself — PostgreSQL renders
                // `(x)[1:array_length(<prefix>, 1)] = <prefix>`, ClickHouse `arraySlice(...)`.
                // `_generate_tree_filter_items` is what sends such a filter.
                if spec.operation == FilterOperation::Startswith && dtype.is_array() {
                    let literal = self.dialect.render_array_literal(&values[0])?;
                    let slice = match self.dialect {
                        Dialect::Postgres => format!("({expr})[1:array_length({literal}, 1)]"),
                        Dialect::ClickHouse => format!("arraySlice({expr}, 1, length({literal}))"),
                        // `dl_formula` has no array `startswith` template for MySQL.
                        Dialect::Mysql => return Err(LiteralError::UnsupportedArray.into()),
                    };
                    return Ok(Some(FilterPart {
                        condition: format!("{slice} = {literal}"),
                        expr,
                        having: is_having,
                        window: is_window,
                        alias: self.dialect.quote_ident(&guid),
                    }));
                }
                let text = crate::literal::literal_to_text(&values[0]);
                let (prefix, suffix) = match spec.operation {
                    FilterOperation::Startswith | FilterOperation::Istartswith => (false, true),
                    FilterOperation::Endswith | FilterOperation::Iendswith => (true, false),
                    _ => (true, true),
                };
                let ignore_case = matches!(
                    spec.operation,
                    FilterOperation::Icontains
                        | FilterOperation::Noticontains
                        | FilterOperation::Istartswith
                        | FilterOperation::Iendswith
                );
                let pattern = like_pattern(&text, prefix, suffix);
                let literal = self.dialect.render_string(&pattern);
                let like = if ignore_case {
                    format!("LOWER({expr}) LIKE LOWER({literal})")
                } else {
                    format!("{expr} LIKE {literal}")
                };
                let negated = matches!(
                    spec.operation,
                    FilterOperation::Notcontains | FilterOperation::Noticontains
                );
                Ok(if negated {
                    format!("NOT ({like})")
                } else {
                    like
                })
            }
            // `LENEQ`..`LENLTE` are `n.binary(op, n.func.LEN(f), val)`: the `len` template of
            // the field's type (for a tree it is the `ARRAY_LENGTH` of the array) compared with
            // an integer. `_mangle_array_filter` casts the value to `INTEGER` for all of them.
            FilterOperation::Leneq
            | FilterOperation::Lenne
            | FilterOperation::Lengt
            | FilterOperation::Lengte
            | FilterOperation::Lenlt
            | FilterOperation::Lenlte => {
                expect("exactly one value", values.len(), "a length comparison")?;
                let integer = DataType::from_native(dfl_native::DataType::Integer);
                let literal = self.dialect.render_literal(&values[0], integer)?;
                let length = self.wrapper("LEN([__wrapped])", &expr, dtype)?;
                let operator = match spec.operation {
                    FilterOperation::Leneq => "=",
                    FilterOperation::Lenne => "!=",
                    FilterOperation::Lengt => ">",
                    FilterOperation::Lengte => ">=",
                    FilterOperation::Lenlt => "<",
                    _ => "<=",
                };
                Ok(format!("{length} {operator} {literal}"))
            }
        };

        Ok(Some(FilterPart {
            expr,
            condition: condition?,
            having: is_having,
            window: is_window,
            alias: self.dialect.quote_ident(&guid),
        }))
    }
}

/// One request filter, compiled.
///
/// The condition is built *from* `expr`, which is why the two are kept together: a filter on a
/// window has to be moved to the query above the window, and the only thing it can refer to
/// there is the alias of that expression.
struct FilterPart {
    /// The field's expression, as the `SELECT` list spells it.
    expr: String,
    condition: String,
    /// The expression aggregates, so the condition is a `HAVING` condition.
    having: bool,
    /// The expression contains a window call, so the condition belongs in neither clause.
    window: bool,
    /// The field's column in the `SELECT` list.
    alias: String,
}

/// The literal of a parameter value in `dl_formula` syntax.
///
/// `dl_formula` literals are dialect independent: strings are double quoted, dates are
/// `#...#`, datetimes `##...##`, booleans `TRUE`/`FALSE`.
fn parameter_formula_literal(value: &serde_json::Value, dtype: DataType) -> Option<String> {
    use serde_json::Value;

    if value.is_null() {
        return Some("NULL".to_string());
    }
    let base = dtype.native().base();
    match base {
        dfl_native::DataType::Integer | dfl_native::DataType::Float => match value {
            Value::Number(number) => Some(number.to_string()),
            Value::String(text) => text.trim().parse::<f64>().ok().map(|n| n.to_string()),
            _ => None,
        },
        dfl_native::DataType::Boolean => match value {
            Value::Bool(flag) => Some(if *flag { "TRUE" } else { "FALSE" }.to_string()),
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Some("TRUE".to_string()),
                "false" | "0" => Some("FALSE".to_string()),
                _ => None,
            },
            _ => None,
        },
        dfl_native::DataType::Date => {
            let text = value.as_str()?;
            Some(format!("#{}#", text.replace('"', "\"\"")))
        }
        dfl_native::DataType::Datetime => {
            let text = value.as_str()?;
            Some(format!("##{}##", text.replace('"', "\"\"")))
        }
        _ => {
            let text = crate::literal::literal_to_text(value);
            Some(format!("\"{}\"", text.replace('"', "\"\"")))
        }
    }
}

/// `%`/`_`/`\` in user input must not turn into wildcards.
fn like_pattern(value: &str, prefix: bool, suffix: bool) -> String {
    let mut pattern = String::with_capacity(value.len() + 2);
    if prefix {
        pattern.push('%');
    }
    for ch in value.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    if suffix {
        pattern.push('%');
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OrderBySpec, SelectItem};
    use dl_core::{FieldRef, Source, UserDataType};
    use serde_json::json;

    fn dataset() -> Dataset {
        let direct =
            |guid: &str, title: &str, dtype: UserDataType, aggregation: Aggregation| Field {
                guid: guid.to_string(),
                title: title.to_string(),
                initial_data_type: Some(dtype),
                data_type: Some(dtype),
                aggregation,
                ..Field::default()
            };
        let mut formula = direct("f_calc", "Calc", UserDataType::Float, Aggregation::None);
        formula.calc_mode = CalcMode::Formula;
        formula.formula = "[Sales] * 2".to_string();
        Dataset {
            sources: vec![Source::table("src", "public.sales")],
            data_export_forbidden: false,
            connection_id: None,
            fields: vec![
                direct(
                    "f_region",
                    "Region",
                    UserDataType::String,
                    Aggregation::None,
                ),
                direct("f_sales", "Sales", UserDataType::Integer, Aggregation::Sum),
                formula,
            ],
            ..Dataset::default()
        }
    }

    fn spec(select: Vec<SelectItem>) -> QuerySpec {
        QuerySpec {
            select,
            ..QuerySpec::default()
        }
    }

    #[test]
    fn a_field_is_a_measure_when_its_final_expression_aggregates() {
        // `FormulaCompiler._process_field_stage_aggregation` fills `_field_types` from
        // `is_aggregate_expression` of the *prepared* formula: references substituted, the
        // field's own aggregation applied. The stored `type` is not consulted — Python
        // recomputes it on every read — so a field that only exists in this request's
        // `updates` still gets the right answer.
        let mut dataset = dataset();
        assert_eq!(dataset.fields[2].field_type, None);
        let types = field_types(&dataset, Dialect::Postgres);
        assert_eq!(types["f_region"], FieldType::Dimension);
        // `sum`: `_apply_aggregation` wraps the column, so the expression aggregates.
        assert_eq!(types["f_sales"], FieldType::Measure);
        // `[Sales] * 2` has no aggregate call of its own, but its reference is substituted
        // aggregated (`SUM(t0."Sales") * 2`), which is what `is_aggregate_expression` sees.
        assert_eq!(types["f_calc"], FieldType::Measure);

        // A saved `type` that contradicts the formula does not win.
        dataset.fields[2].field_type = Some(FieldType::Dimension);
        assert_eq!(
            field_types(&dataset, Dialect::Postgres)["f_calc"],
            FieldType::Measure
        );
    }

    #[test]
    fn a_window_call_is_a_measure_only_through_its_arguments() {
        // The captured `Window functions: Rank` chart adds `RANK([Sales])` by `updates` and
        // Python answers `field_type: MEASURE`. `is_aggregate_expression` never counts the
        // window call itself, but it does walk its arguments — and `[Sales]` is substituted as
        // `SUM(sales)`. `RANK` over a dimension has no aggregate anywhere and stays one.
        let mut dataset = dataset();
        let mut rank = Field {
            guid: "f_rank".to_string(),
            title: "Rank".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "RANK([Sales])".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        };
        dataset.fields.push(rank.clone());
        assert_eq!(
            field_types(&dataset, Dialect::Postgres)["f_rank"],
            FieldType::Measure
        );
        rank.formula = "RANK([Region])".to_string();
        *dataset.fields.last_mut().unwrap() = rank;
        // A dimension, not a fallback through a compile error: the formula compiles fine.
        assert!(
            check_formulas(&dataset, Dialect::Postgres).is_empty(),
            "{:?}",
            check_formulas(&dataset, Dialect::Postgres)
        );
        assert_eq!(
            field_types(&dataset, Dialect::Postgres)["f_rank"],
            FieldType::Dimension
        );
    }
    #[test]
    fn an_unforked_request_keeps_the_compilers_sql() {
        // The plan is always built, but a request without a LOD clause has no fork: the
        // leaves go back into their places and the SQL stays byte for byte what it was.
        let compiled = compile(
            &dataset(),
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_sales"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\""
        );

        // A formula dimension is folded into one expression before the plan is built, and the
        // plan inlines it back: the `SELECT` list and the `GROUP BY` see the same expression,
        // not a column of a sub-query. (`[Sales]` is an aggregated measure, so the formula is
        // one too and does not take part in the `GROUP BY`.)
        let compiled = compile(
            &dataset(),
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_calc"), Role::Row),
            ]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\", (SUM(t0.\"Sales\") * 2) AS \"f_calc\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\""
        );
    }

    #[test]
    fn a_lod_by_a_dimension_joins_on_it() {
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_lod".to_string(),
            title: "Sales by region".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "COUNTD([Sales] FIXED [Region])".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let compiled = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_lod"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("a LOD measure by a dimension compiles into a fork");
        println!("{}", compiled.sql);
        assert!(!compiled.sql.contains("{leaf:"), "{}", compiled.sql);
        // The base groups by the dimension, and the fork is joined on it, `NULL`s included.
        assert!(
            compiled.sql.contains("GROUP BY t0.\"Region\""),
            "{}",
            compiled.sql
        );
        assert!(compiled.sql.contains(" OR (q_0."), "{}", compiled.sql);
        assert!(compiled.sql.contains(" IS NULL AND "), "{}", compiled.sql);
    }

    #[test]
    fn a_lod_measure_becomes_a_forked_subquery() {
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_lod".to_string(),
            title: "Regions".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "COUNTD([Region] FIXED)".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_lod"), Role::Measure)]),
            Dialect::Postgres,
        )
        .expect("a LOD measure compiles into a fork");
        println!("{}", compiled.sql);
        assert!(!compiled.sql.contains("{leaf:"), "{}", compiled.sql);
        // No dimensions: the fork is one row and there is no key to match, so it is a
        // `CROSS JOIN` (a constant `ON` would re-run the fork once per row of the base).
        assert!(
            compiled.sql.contains("CROSS JOIN (SELECT 1 AS"),
            "{}",
            compiled.sql
        );
        assert!(compiled.sql.contains("count(DISTINCT"), "{}", compiled.sql);
    }

    #[test]
    fn a_lod_over_an_aggregated_field_does_not_nest_aggregates() {
        // The demo's local measure `AVG([Delivered, %] FIXED [Region])`, where `Delivered, %`
        // is a formula that already aggregates: `COUNTD(...) / COUNTD(...)`. The LOD aggregate
        // cannot wrap that in one query (`avg(count(...) / count(...))` is SQLSTATE 42803), so
        // the fork computes the ratio at its own grouping and the aggregate reads those rows.
        let mut dataset = dataset();
        let mut pct = Field {
            guid: "f_pct".to_string(),
            title: "Delivered, %".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "COUNTD([Region]) / COUNTD([Sales])".to_string(),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        pct.cast = None;
        dataset.fields.push(pct);
        let mut lod = Field {
            guid: "f_lod".to_string(),
            title: "Delivered by region".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AVG([Delivered, %] FIXED [Region])".to_string(),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        lod.cast = None;
        dataset.fields.push(lod);

        let sql = compile(
            &dataset,
            &QuerySpec {
                select: vec![
                    SelectItem::new(FieldRef::id("f_region"), Role::Row),
                    SelectItem::new(FieldRef::id("f_lod"), Role::Measure),
                ],
                // The request filter stays with the query that reads the source: the outer
                // query of the fork only sees the inner query's columns.
                filters: vec![FilterSpec {
                    field_ref: FieldRef::id("f_region"),
                    operation: FilterOperation::In,
                    values: vec![serde_json::json!("north")],
                }],
                ..QuerySpec::default()
            },
            Dialect::Postgres,
        )
        .expect("the LOD compiles into a two-level fork")
        .sql;
        println!("{sql}");
        let plain = sql.to_lowercase();
        // One aggregate per query: the ratio in the inner fork, `avg` in the outer one.
        assert!(!plain.contains("avg(count("), "{sql}");
        assert!(!plain.contains("avg((count("), "{sql}");
        assert!(plain.contains("avg(f_0."), "{sql}");
        assert!(plain.contains("count(distinct t0.\"region\")"), "{sql}");
        // The filter is applied where the source is read — the base query and the fork's inner
        // query — and not again in the level above them, which only sees the fork's columns.
        assert_eq!(plain.matches("where t0.\"region\"").count(), 2, "{sql}");
        // Both levels group by the LOD dimension, so the join stays one row per region.
        assert_eq!(plain.matches("group by").count(), 3, "{sql}");
    }

    #[test]
    fn an_ungrouped_lod_over_an_aggregated_field_keeps_its_constant_key() {
        // `AVG([Delivered, %] FIXED)`: the level is the whole dataset, so the fork is one row
        // and still has to be two queries — the argument aggregates by itself.
        let mut dataset = dataset();
        let mut pct = Field {
            guid: "f_pct".to_string(),
            title: "Pct".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "SUM([Sales]) / COUNTD([Region])".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        pct.cast = None;
        dataset.fields.push(pct);
        let mut lod = Field {
            guid: "f_lod".to_string(),
            title: "Pct total".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AVG([Pct] FIXED)".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        lod.cast = None;
        dataset.fields.push(lod);

        let sql = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_lod"), Role::Measure)]),
            Dialect::Postgres,
        )
        .expect("the ungrouped LOD compiles into a two-level fork")
        .sql;
        println!("{sql}");
        let plain = sql.to_lowercase();
        assert!(!plain.contains("avg(sum("), "{sql}");
        assert!(!plain.contains("avg((sum("), "{sql}");
        assert!(plain.contains("avg(f_0."), "{sql}");
        // The inner query has no `GROUP BY`: the whole dataset is one row, like an ungrouped
        // LOD, and the constant key keeps it a `CROSS JOIN`.
        assert!(plain.contains("cross join (select 1 as"), "{sql}");
    }

    #[test]
    fn an_include_lod_over_an_aggregated_field_does_not_nest_aggregates() {
        // `AVG(COUNTD([Client ID] INCLUDE [Order date]))` from the demo's "LOD: INCLUDE" chart:
        // `INCLUDE` adds a dimension to the base grouping and the `avg` is applied above the
        // fork, over one `COUNTD` value per day.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_date".to_string(),
            title: "Order date".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_month".to_string(),
            title: "Order month".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "DATETRUNC([Order date], \"month\")".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_avg".to_string(),
            title: "Avg clients per day".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AVG(COUNTD([Region] INCLUDE [Order date]))".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        });

        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_month"), Role::Row),
                SelectItem::new(FieldRef::id("f_avg"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("the INCLUDE LOD compiles above the fork")
        .sql;
        println!("{sql}");
        let plain = sql.to_lowercase();
        // The one `COUNTD` stays inside the fork; the `avg` reads the fork's column.
        assert!(!plain.contains("avg(count("), "{sql}");
        assert_eq!(plain.matches("count(distinct").count(), 1, "{sql}");
        assert!(plain.contains("avg(f_0."), "{sql}");
        // `INCLUDE` groups by the request's dimension *and* the included one.
        assert!(plain.contains("\"order date\""), "{sql}");
        assert!(plain.contains("date_trunc"), "{sql}");
    }

    #[test]
    fn an_include_lod_with_an_aggregated_argument_does_not_nest_aggregates() {
        // The same nesting rule for `INCLUDE`: the argument aggregates, so the call needs a
        // query of its own before the level above can average over the included dimension.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_date".to_string(),
            title: "Order date".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        let mut pct = Field {
            guid: "f_pct".to_string(),
            title: "Pct".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "SUM([Sales]) / COUNTD([Region])".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        pct.cast = None;
        dataset.fields.push(pct);
        let mut lod = Field {
            guid: "f_lod".to_string(),
            title: "Pct per date".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AVG([Pct] INCLUDE [Order date])".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        lod.cast = None;
        dataset.fields.push(lod);

        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_date"), Role::Row),
                SelectItem::new(FieldRef::id("f_lod"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("the INCLUDE LOD compiles into a two-level fork")
        .sql;
        println!("{sql}");
        let plain = sql.to_lowercase();
        assert!(!plain.contains("avg(sum("), "{sql}");
        assert!(!plain.contains("avg((sum("), "{sql}");
        assert!(plain.contains("avg(f_0."), "{sql}");
        // `INCLUDE` groups the inner query by the request's dimension and the included one.
        assert!(plain.contains("group by"), "{sql}");
    }

    #[test]
    fn a_before_filter_by_window_forks_without_its_filter() {
        // `SUM([Sales] WITHIN [Region] BEFORE FILTER BY [Category])`: the window partitions by
        // the region but sees the rows before the request filtered the category, so the fork
        // groups by the region and keeps no `WHERE` on the category.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_category".to_string(),
            title: "Category".to_string(),
            initial_data_type: Some(UserDataType::String),
            data_type: Some(UserDataType::String),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_window".to_string(),
            title: "Region share".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "SUM([Sales] WITHIN [Region] BEFORE FILTER BY [Category])".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let spec = QuerySpec {
            select: vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_window"), Role::Measure),
            ],
            filters: vec![FilterSpec {
                field_ref: FieldRef::id("f_category"),
                operation: FilterOperation::In,
                values: vec![serde_json::json!("Auto")],
            }],
            ..QuerySpec::default()
        };
        let sql = compile(&dataset, &spec, Dialect::Postgres)
            .expect("a BEFORE FILTER BY window compiles into a fork")
            .sql;
        println!("{sql}");
        let fork = sql
            .split("JOIN (SELECT")
            .nth(1)
            .expect("the window is a join, not an inline expression");
        assert!(!fork.contains("Category"), "{sql}");
        assert!(sql.contains("WHERE t0.\"Category\" IN ('Auto')"), "{sql}");
        assert!(sql.contains("sum(t0.\"Sales\")"), "{sql}");
    }

    #[test]
    fn a_lookup_forks_a_left_join_on_the_offset_dimension() {
        // `AGO([Sales], [OrderDate], 'month', 1)`: `dl_formula` has no translation for the
        // lookup, `LookupFunctionToQueryForkMutation` makes it the measure plus a `LEFT JOIN`
        // of the same grouped query shifted by `DATEADD`, and the month-based unit also has to
        // match the day of the month so that a 30-day February does not lend its value twice.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_date".to_string(),
            title: "OrderDate".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_ago".to_string(),
            title: "Sales AGO".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AGO([Sales], [OrderDate], 'month', 1)".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_date"), Role::Row),
                SelectItem::new(FieldRef::id("f_sales"), Role::Measure),
                SelectItem::new(FieldRef::id("f_ago"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("a lookup compiles into a fork")
        .sql;
        println!("{sql}");
        assert!(!sql.contains("{leaf:"), "{sql}");
        // The previous value is a join, and the rows without one are kept.
        assert!(sql.contains("LEFT JOIN"), "{sql}");
        let join = sql.split("LEFT JOIN").nth(1).expect("the lookup is a join");
        // The offset is on the fork's side of the equality, the same direction Python emits.
        assert!(
            join.contains("INTERVAL '1 MONTH'"),
            "the fork is shifted by the unit: {sql}"
        );
        assert!(join.contains("DATE_PART('day'"), "the day matches: {sql}");
        // ... and the fork is grouped by every dimension of the request, or the previous month
        // of one region would be joined to another region's rows.
        assert!(join.contains("\"Region\""), "{sql}");
    }

    #[test]
    fn at_date_forks_a_left_join_on_its_date_argument() {
        // `AT_DATE([Sales], [OrderDate], #2014-02-02#)`: `AtDateLookupFunctionMutator` joins
        // the base query to the fork on `date_argument = fork.<date dimension>` — no `DATEADD`
        // offset — and keeps the rows without a match, like `AGO`.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_date".to_string(),
            title: "OrderDate".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_at".to_string(),
            title: "Sales at date".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AT_DATE([Sales], [OrderDate], #2014-02-02#)".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_date"), Role::Row),
                SelectItem::new(FieldRef::id("f_sales"), Role::Measure),
                SelectItem::new(FieldRef::id("f_at"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("an AT_DATE compiles into a fork")
        .sql;
        println!("{sql}");
        assert!(!sql.contains("{leaf:"), "{sql}");
        assert!(sql.contains("LEFT JOIN"), "{sql}");
        let join = sql.split("LEFT JOIN").nth(1).expect("the lookup is a join");
        // The base side is the date argument, not the offset dimension.
        assert!(
            join.contains("CAST('2014-02-02' AS DATE)"),
            "the date argument is the join key: {sql}"
        );
        assert!(!join.contains("INTERVAL"), "AT_DATE has no offset: {sql}");
        // The fork is grouped by every dimension of the request, as for `AGO`.
        assert!(join.contains("\"Region\""), "{sql}");
    }

    #[test]
    fn an_aggregate_over_a_fork_gets_a_query_above_it() {
        // `AVG(SUM([Sales] INCLUDE [Region]))` — the demo's "Charts: Combo with average". The
        // inner `SUM` is a fork grouped by the region *and* the month; the `AVG` averages its
        // rows per month, so it cannot be in the same query (`avg(sum(x))` is not SQL).
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_date".to_string(),
            title: "OrderDate".to_string(),
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_avg".to_string(),
            title: "Avg by region".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "AVG(SUM([Sales] INCLUDE [Region]))".to_string(),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        });
        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_date"), Role::Row),
                SelectItem::new(FieldRef::id("f_avg"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("an aggregate over a fork compiles")
        .sql;
        println!("{sql}");
        // The fork itself is grouped by the month *and* the included region ...
        let fork = sql
            .split("JOIN (SELECT")
            .nth(1)
            .expect("the INCLUDE LOD is a fork");
        assert!(fork.contains("GROUP BY"), "{sql}");
        assert!(fork.contains("\"Region\""), "{sql}");
        // ... and the average is taken in a query of its own, above the fork.
        assert!(!sql.contains("avg(sum("), "{sql}");
        assert!(
            sql.contains("avg(f_"),
            "the average is over the fork: {sql}"
        );
    }

    #[test]
    fn a_window_over_a_window_gets_a_query_of_its_own() {
        // The demo's "Cohorts: Abs and Retention": `[precalc] / MAX([precalc] AMONG [Region])`
        // with `precalc` = `SUM([Sales]) / RMAX(SUM([Sales]) AMONG [Region])`. The outer window
        // is built from the value the inner one computes, and no dialect allows that in one
        // `SELECT` — Postgres answers `window function calls cannot be nested` (SQLSTATE 42P20).
        // `WinFuncQuerySplitter` keeps a window function as the topmost node of its query, so the
        // inner one becomes a fork and the outer one reads the column it selects.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_precalc".to_string(),
            title: "per region".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "SUM([Sales]) / RMAX(SUM([Sales]) AMONG [Region])".to_string(),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        });
        dataset.fields.push(Field {
            guid: "f_top".to_string(),
            title: "per month".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "[per region] / MAX([per region] AMONG [Region])".to_string(),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        });
        let sql = compile(
            &dataset,
            &spec(vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_top"), Role::Measure),
            ]),
            Dialect::Postgres,
        )
        .expect("a window over a window compiles")
        .sql;
        println!("{sql}");
        assert!(!window_over_window(&sql), "{sql}");
        // The inner window is a query of its own ...
        assert!(sql.contains("JOIN (SELECT"), "{sql}");
        // ... and the outer one reads the column that query selected.
        assert!(sql.contains(") OVER (PARTITION BY"), "{sql}");
    }

    /// Whether the SQL has a window inside another window's argument: `max(... OVER (...) ) OVER
    /// (...)`. Found by looking at what the group in front of every `) OVER (` holds.
    fn window_over_window(sql: &str) -> bool {
        let bytes = sql.as_bytes();
        let mut open: Vec<usize> = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'(' {
                open.push(index);
            } else if bytes[index] == b')' {
                let Some(start) = open.pop() else {
                    index += 1;
                    continue;
                };
                if sql[index..].starts_with(") OVER (") {
                    // The group that just closed is the window's argument: a window in it means
                    // one window is nested in another, which no dialect accepts.
                    if sql[start..index].contains(" OVER (") {
                        return true;
                    }
                }
            }
            index += 1;
        }
        false
    }

    #[test]
    fn a_filter_on_a_window_wraps_the_query() {
        // `RANK([Sales]) <= 5` — the demo's "Window functions: Rank" chart. A window is only
        // there once the rows are grouped, so the condition can be neither a `WHERE` nor a
        // `HAVING`: `QueryForkQuerySplitter` filters the query above the window fork, and the
        // filter refers to the window's column by its alias.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_rank".to_string(),
            title: "Sales RANK".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "RANK([Sales])".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        });
        let mut spec = spec(vec![
            SelectItem::new(FieldRef::id("f_region"), Role::Row),
            SelectItem::new(FieldRef::id("f_rank"), Role::Measure),
            SelectItem::new(FieldRef::id("f_sales"), Role::Measure),
        ]);
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_rank"),
            operation: FilterOperation::Lte,
            values: vec![serde_json::json!(5)],
        }];
        spec.order_by = vec![OrderBySpec {
            field_ref: FieldRef::id("f_sales"),
            direction: Direction::Desc,
        }];
        spec.limit = Some(100);
        let sql = compile(&dataset, &spec, Dialect::Postgres)
            .expect("a filter on a window compiles")
            .sql;
        println!("{sql}");
        // The window lives in the sub-query ...
        let (inner, outer) = sql
            .split_once(") w_0 WHERE")
            .expect("the window is in a sub-query");
        assert!(inner.contains("rank() OVER ("), "{sql}");
        // ... and the filter compares the column of the sub-query.
        assert!(!inner.contains("f_rank\" <= 5"), "{sql}");
        assert!(outer.contains("f_rank\" <= 5"), "{sql}");
        // The request's page belongs to the filtered rows, not to the window's input.
        assert!(outer.contains("ORDER BY 3 DESC LIMIT 100"), "{sql}");
        assert!(!inner.contains("LIMIT"), "{sql}");
    }

    #[test]
    fn a_stale_source_avatar_is_only_an_error_when_the_field_is_used() {
        // `QueryFormalizer.validate_select_field` resolves a direct field's `avatar_id` for
        // the fields a block selects, so an unused field with a stale avatar is not an error:
        // the demo's `Charts: Pie chart` request carries one in `updates` and Python answers it.
        let mut dataset = dataset();
        let mut stale = Field {
            guid: "f_stale".to_string(),
            title: "Product subcategory".to_string(),
            avatar_id: Some("gone".to_string()),
            source: "ProductSubcategory".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            aggregation: Aggregation::Sum,
            ..Field::default()
        };
        dataset.fields.push(stale.clone());

        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_region"), Role::Row)]),
            Dialect::Postgres,
        )
        .expect("an unused field is not validated");
        assert!(compiled.sql.contains("\"Region\""), "{}", compiled.sql);

        // Selecting it *is* the error `_avatar_exists` raises.
        let error = compile(
            &dataset,
            &spec(vec![SelectItem::new(
                FieldRef::id("f_stale"),
                Role::Measure,
            )]),
            Dialect::Postgres,
        )
        .expect_err("a selected stale field is rejected");
        assert!(
            matches!(&error, QueryError::Core(CoreError::UnknownSource { guid, .. }) if guid == "f_stale"),
            "{error:?}"
        );

        // A removed avatar that is no longer stale is fine again.
        stale.avatar_id = Some("src".to_string());
        *dataset.fields.last_mut().unwrap() = stale;
        compile(
            &dataset,
            &spec(vec![SelectItem::new(
                FieldRef::id("f_stale"),
                Role::Measure,
            )]),
            Dialect::Postgres,
        )
        .expect("the field resolves now");
    }

    #[test]
    fn a_direct_field_reads_its_type_from_the_field_and_its_column_from_the_schema() {
        // The casting stage compares the field's own `data_type` (`DatasetValidator` keeps it
        // up to date from the request), so a `string` column read by an `integer` field is
        // `count(DISTINCT ...)`, not `count(DISTINCT CAST(... AS INTEGER))` — the demo's
        // `order_id` is a `text` column and Python does not cast it.
        //
        // The schema is still what says whether the column exists: `Продажа, ₽` points at a
        // `Price` column the `orders` table does not have, and Python reports the field with
        // the stage manager's `string` fallback (`DEFAULT_DATA_TYPE`) because of it.
        let mut dataset = dataset();
        dataset.sources = vec![Source {
            id: "src".to_string(),
            table: "public.sales".to_string(),
            connection_id: None,
            subselect: None,
            columns: vec![
                dl_core::SourceColumn {
                    name: "Sales".to_string(),
                    data_type: UserDataType::Float,
                },
                dl_core::SourceColumn {
                    name: "Region".to_string(),
                    data_type: UserDataType::String,
                },
            ],
        }];
        let types = field_data_types(&dataset, Dialect::Postgres);
        // Declared `integer` in the dataset, `float` in the schema: the field wins.
        assert_eq!(types["f_sales"], UserDataType::Integer);
        assert_eq!(types["f_region"], UserDataType::String);

        let mut missing = Field {
            guid: "f_missing".to_string(),
            title: "Missing".to_string(),
            source: "Price".to_string(),
            avatar_id: Some("src".to_string()),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            aggregation: Aggregation::Sum,
            ..Field::default()
        };
        dataset.fields.push(missing.clone());
        assert_eq!(
            field_data_types(&dataset, Dialect::Postgres)["f_missing"],
            UserDataType::String
        );
        let error = compile(
            &dataset,
            &spec(vec![SelectItem::new(
                FieldRef::id("f_missing"),
                Role::Measure,
            )]),
            Dialect::Postgres,
        )
        .expect_err("the column does not exist");
        assert!(
            matches!(&error, QueryError::Core(CoreError::UnknownColumn { guid, column, .. })
                if guid == "f_missing" && column == "Price"),
            "{error:?}"
        );

        // With a column that does exist the field is read as its own type again.
        missing.source = "Sales".to_string();
        *dataset.fields.last_mut().unwrap() = missing;
        assert_eq!(
            field_data_types(&dataset, Dialect::Postgres)["f_missing"],
            UserDataType::Float
        );
    }

    #[test]
    fn a_formula_type_is_known_before_its_query_fork_is_built() {
        // `get_field_final_data_type` asks for a type, not for SQL: Python knows that
        // `[Sales] / SUM([Sales] TOTAL)` is a float long before `dl_query_processing` lifts
        // the `TOTAL` into a sub-query, and the legend reports that float. The port still
        // reports the SQL gap, but the type must not depend on it.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_share".to_string(),
            title: "Share".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "[Sales] / SUM([Sales] TOTAL)".to_string(),
            cast: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        });
        assert_eq!(
            field_data_types(&dataset, Dialect::Postgres)["f_share"],
            UserDataType::Float
        );
        // `TOTAL` is a query fork now, not a gap: the compiler lifts `SUM([Sales] TOTAL)` into
        // a sub-query of its own and joins it back on a constant key.
        assert!(check_formulas(&dataset, Dialect::Postgres).is_empty());
        let spec = QuerySpec {
            select: vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_share"), Role::Measure),
            ],
            ..QuerySpec::default()
        };
        let sql = compile(&dataset, &spec, Dialect::Postgres).unwrap().sql;
        assert!(sql.contains("CROSS JOIN (SELECT 1 AS"), "{sql}");
        assert!(sql.contains("sum(t0.\"Sales\")"), "{sql}");
    }

    #[test]
    fn a_formula_reference_to_a_missing_field_names_it() {
        // A reference to a deleted field has to say so
        // (`ERR.DS_API.FORMULA.UNKNOWN_FIELD_IN_FORMULA`), not fail later with
        // "no translation for -(FLOAT|STRING)".
        let mut dataset = dataset();
        let bad = Field {
            guid: "f_bad".to_string(),
            title: "Bad".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "[Nope] + 1".to_string(),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        dataset.fields.push(bad);
        let error = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_bad"), Role::Measure)]),
            Dialect::Postgres,
        )
        .unwrap_err();
        assert!(
            matches!(error, QueryError::UnknownFieldInFormula(ref name) if name == "Nope"),
            "{error}"
        );
    }

    #[test]
    fn a_window_over_a_fork_is_computed_over_the_fork_rows() {
        // `RMAX([Sales] AMONG [Region])`: the running maximum is a window over the fork (the
        // per-region totals), and the wrapper level reads the fork alone. Joining the source
        // rows to it again is what made PostgreSQL sort 11 780 rows for 56 groups in the cohort
        // chart; the window has to see the fork's rows.
        let mut dataset = dataset();
        let mut running = Field {
            guid: "f_running".to_string(),
            title: "Running".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "RMAX(RSUM([Sales]))".to_string(),
            data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        dataset.fields.push(running.clone());
        running.cast = None;
        let spec = QuerySpec {
            select: vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_running"), Role::Measure),
            ],
            ..QuerySpec::default()
        };
        let sql = compile(&dataset, &spec, Dialect::Postgres).unwrap().sql;
        let plain = sql.to_lowercase();
        assert!(plain.contains("over ("), "{sql}");
        // The window reads the fork's rows: its partition is a fork column, not a source
        // expression, and the wrapper selects from the fork subquery.
        // The wrapper level reads the fork's rows: the window argument is a fork column and
        // the source table is not joined to it a second time.
        assert!(plain.contains("max(f_0."), "{sql}");
        assert!(plain.contains("over (rows between"), "{sql}");
    }

    #[test]
    fn a_sum_over_an_aggregated_field_does_not_nest_aggregates() {
        // A `SUM([Revenue]) - SUM([Cost]) - SUM([Ads])` chart over formulas that already
        // aggregate: the outer `SUM` has one value per group, so it is dropped instead of
        // rendering `SUM(sum(...))`, which both PostgreSQL and ClickHouse reject.
        let mut dataset = dataset();
        let mut storage = Field {
            guid: "f_storage".to_string(),
            title: "Storage".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "sum([Sales])".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        storage.cast = None;
        dataset.fields.push(storage);
        let mut check = Field {
            guid: "f_check".to_string(),
            title: "Check".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "SUM([Sales]) - SUM([Storage])".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        check.cast = None;
        dataset.fields.push(check);

        let spec = QuerySpec {
            select: vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_check"), Role::Measure),
            ],
            ..QuerySpec::default()
        };
        let sql = compile(&dataset, &spec, Dialect::Postgres).unwrap().sql;
        let plain = sql.to_lowercase();
        assert!(!plain.contains("sum(sum("), "{sql}");
        assert_eq!(plain.matches("sum(").count(), 2, "{sql}");
        // Both the direct `SUM([Sales])` and the aggregated reference stay as one call each.
        assert!(plain.contains("sum(t0.\"sales\")"), "{sql}");
    }

    #[test]
    fn a_total_over_a_non_distributive_aggregate_aggregates_the_grouped_rows() {
        // The demo's "Diff to average": `AVG([Sales] TOTAL)` averages one value per category,
        // not the thousands of source rows. `SUM([Sales] TOTAL)` is distributive and keeps the
        // single-query fork, so only the non-distributive functions become two queries.
        let mut dataset = dataset();
        let diff = Field {
            guid: "f_diff".to_string(),
            title: "Diff".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "([Sales] - AVG([Sales] TOTAL)) / AVG([Sales] TOTAL)".to_string(),
            data_type: Some(UserDataType::Float),
            initial_data_type: Some(UserDataType::Float),
            ..Field::default()
        };
        dataset.fields.push(diff);

        let spec = QuerySpec {
            select: vec![
                SelectItem::new(FieldRef::id("f_region"), Role::Row),
                SelectItem::new(FieldRef::id("f_diff"), Role::Measure),
            ],
            ..QuerySpec::default()
        };
        let sql = compile(&dataset, &spec, Dialect::Postgres).unwrap().sql;
        let plain = sql.to_lowercase();
        // The inner fork aggregates per request dimension...
        assert!(plain.contains("group by t0.\"region\""), "{sql}");
        assert!(plain.contains("sum(t0.\"sales\") as"), "{sql}");
        // ...and the outer one averages those grouped values, joined to the base by a constant.
        // The constant key is a one-row relation, so it is a `CROSS JOIN`, not `JOIN ... ON`.
        assert!(plain.contains("avg(f_0."), "{sql}");
        assert!(sql.contains("CROSS JOIN (SELECT 1 AS"), "{sql}");
    }

    #[test]
    fn a_field_cast_is_applied_to_the_prepared_formula() {
        // `_process_field_stage_casting` runs between substitution and aggregation: the
        // dataset's `cast` is applied to the expression the formula produced, as the formula
        // function of that type. The demo's `Month` field is declared `date` and its formula
        // yields a timestamp, so Python queries `CAST(date_trunc(...) AS DATE)` — without the
        // cast the row decoder hands the frontend `2020-02-01T00:00:00`, not `2020-02-01`.
        let mut dataset = dataset();
        let mut month = Field {
            guid: "f_month".to_string(),
            title: "Month".to_string(),
            calc_mode: CalcMode::Formula,
            formula: "NOW()".to_string(),
            cast: Some(UserDataType::Date),
            // The declared type is what `_apply_cast` compares against: `date`, while the
            // formula's own type is `GENERICDATETIME`.
            initial_data_type: Some(UserDataType::Date),
            data_type: Some(UserDataType::Date),
            ..Field::default()
        };
        dataset.fields.push(month.clone());
        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_month"), Role::Row)]),
            Dialect::Postgres,
        )
        .expect("the casted formula compiles");
        assert_eq!(
            compiled.sql,
            "SELECT (CAST(now() AS DATE)) AS \"f_month\" FROM \"public\".\"sales\" AS t0"
        );

        // A formula that already has the declared type is not wrapped: `date()` would be a
        // no-op `CAST(... AS DATE)` in the SQL and Python does not emit it.
        month.formula = "DATE(NOW())".to_string();
        *dataset.fields.last_mut().unwrap() = month;
        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem::new(FieldRef::id("f_month"), Role::Row)]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(compiled.sql.matches("CAST(").count(), 1, "{}", compiled.sql);
    }

    #[test]
    fn a_field_whose_formula_does_not_compile_keeps_its_stored_type() {
        // `get_field_type` answers `_field_types.get(guid, DIMENSION)`: a field the compiler
        // never got to is a dimension, and the saved schema is the next best evidence.
        let mut dataset = dataset();
        dataset.fields[2].formula = "SUM(".to_string();
        assert_eq!(
            field_types(&dataset, Dialect::Postgres)["f_calc"],
            FieldType::Dimension
        );
        dataset.fields[2].field_type = Some(FieldType::Measure);
        assert_eq!(
            field_types(&dataset, Dialect::Postgres)["f_calc"],
            FieldType::Measure
        );
    }

    #[test]
    fn dimensions_and_measures_become_select_group_by() {
        let spec = spec(vec![
            SelectItem {
                field_ref: FieldRef::id("f_region"),
                role: Role::Row,
                level: None,
            },
            SelectItem {
                field_ref: FieldRef::id("f_sales"),
                role: Role::Measure,
                level: None,
            },
        ]);
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\""
        );
        assert!(compiled.grouped);
        assert_eq!(compiled.columns[1].aggregation, Aggregation::Sum);
    }

    #[test]
    fn a_custom_sql_source_becomes_a_subquery_in_the_from_clause() {
        // `SubselectDataSource.get_sql_source`: `(\n{subsql}\n) AS <alias>`, verbatim — a
        // "Custom SQL" dataset has no table to name, and the demo's Cohorts dataset is one.
        let mut dataset = dataset();
        dataset.sources = vec![Source {
            id: "src".to_string(),
            table: String::new(),
            connection_id: None,
            subselect: Some("SELECT user_id, amount FROM public.orders".to_string()),
            columns: Vec::new(),
        }];
        let spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_sales"),
            role: Role::Measure,
            level: None,
        }]);
        let compiled = compile(&dataset, &spec, Dialect::Postgres).unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM (\nSELECT user_id, amount FROM public.orders\n) AS t0"
        );
    }

    #[test]
    fn formulas_go_through_the_native_translator() {
        let spec = spec(vec![SelectItem {
            field_ref: FieldRef::title("Calc"),
            role: Role::Measure,
            level: None,
        }]);
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        // `[Sales]` is a measure of the dataset, so its reference is substituted *aggregated*
        // (`_substitute_fields_in_formula`): the formula is `SUM(sales) * 2`, a measure even
        // though `Calc` itself has no aggregation and its own text has no aggregate call.
        // The aggregation of a dataset field is rendered by `aggregate_sql()`, so it keeps the
        // uppercase spelling; only calls written inside the formula come from the `dl_formula`
        // templates (which spell them lowercase for postgres).
        assert_eq!(
            compiled.sql,
            "SELECT (SUM(t0.\"Sales\") * 2) AS \"f_calc\" FROM \"public\".\"sales\" AS t0"
        );
        // Nothing to group by: the `SELECT` list is one aggregated expression.
        assert!(!compiled.grouped);
        assert_eq!(compiled.columns[0].data_type.name(), "INTEGER");
    }

    #[test]
    fn an_aggregate_inside_a_formula_keeps_its_reference_unaggregated() {
        // `SUM([Sales])` must not become `SUM(SUM(...))`: `_suppress_double_aggregations` keeps
        // the reference at the `casting` stage when it sits inside an aggregate call.
        let mut dataset = dataset();
        let mut formula = Field::default();
        formula.guid = "f_sum".to_string();
        formula.title = "Sum".to_string();
        formula.initial_data_type = Some(UserDataType::Integer);
        formula.data_type = Some(UserDataType::Integer);
        formula.calc_mode = CalcMode::Formula;
        formula.formula = "SUM([Sales])".to_string();
        dataset.fields.push(formula);

        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem {
                field_ref: FieldRef::id("f_sum"),
                role: Role::Measure,
                level: None,
            }]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT (sum(t0.\"Sales\")) AS \"f_sum\" FROM \"public\".\"sales\" AS t0"
        );
        assert!(!compiled.grouped);
    }

    #[test]
    fn a_reference_inside_and_outside_an_aggregate_is_substituted_per_occurrence() {
        // `dl_formula` keys the substitutions by node index, so the outer `[Sales]` gets the
        // aggregated expression while the one inside `SUM()` keeps the column: `SUM(x) / SUM(x)`,
        // never `SUM(SUM(x))`.
        let mut dataset = dataset();
        let mut formula = Field::default();
        formula.guid = "f_ratio".to_string();
        formula.title = "Ratio".to_string();
        formula.initial_data_type = Some(UserDataType::Float);
        formula.data_type = Some(UserDataType::Float);
        formula.calc_mode = CalcMode::Formula;
        formula.formula = "[Sales] / SUM([Sales])".to_string();
        dataset.fields.push(formula);

        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem {
                field_ref: FieldRef::id("f_ratio"),
                role: Role::Measure,
                level: None,
            }]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT (CAST(SUM(t0.\"Sales\") AS DOUBLE PRECISION) / sum(t0.\"Sales\")) \
             AS \"f_ratio\" FROM \"public\".\"sales\" AS t0"
        );
    }

    #[test]
    fn filters_order_and_limit_are_rendered_per_dialect() {
        let mut spec = spec(vec![
            SelectItem {
                field_ref: FieldRef::id("f_region"),
                role: Role::Row,
                level: None,
            },
            SelectItem {
                field_ref: FieldRef::id("f_sales"),
                role: Role::Measure,
                level: None,
            },
        ]);
        spec.filters = vec![
            FilterSpec {
                field_ref: FieldRef::id("f_region"),
                operation: FilterOperation::Contains,
                values: vec![json!("It's")],
            },
            FilterSpec {
                field_ref: FieldRef::id("f_sales"),
                operation: FilterOperation::Between,
                values: vec![json!(10), json!(20)],
            },
        ];
        spec.order_by = vec![OrderBySpec {
            field_ref: FieldRef::id("f_sales"),
            direction: Direction::Desc,
        }];
        spec.limit = Some(100);

        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert_eq!(
            compiled.sql,
            // The measure filter compares `SUM(Sales)`, so it is a `HAVING`, not a `WHERE`.
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM \"public\".\"sales\" AS t0 \
             WHERE t0.\"Region\" LIKE '%It''s%' \
             GROUP BY t0.\"Region\" HAVING SUM(t0.\"Sales\") BETWEEN 10 AND 20 \
             ORDER BY 2 DESC LIMIT 100"
        );
        let mysql = compile(&dataset(), &spec, Dialect::Mysql).unwrap();
        assert!(mysql.sql.contains("FROM `public`.`sales` AS t0"));
        assert!(mysql.sql.contains("LIKE '%It''s%'"));
    }

    #[test]
    fn in_filters_keep_the_null_rows() {
        // `dl_formula` renders `in`/`notin` through `_in_fix_null`/`_not_in_fix_null`: a bare
        // `x NOT IN (...)` drops the rows where `x` is NULL, so the condition carries an
        // `OR x IS NULL` branch. A `null` inside the list is not compared with either.
        let mut spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_region"),
            role: Role::Row,
            level: None,
        }]);
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_region"),
            operation: FilterOperation::Nin,
            values: vec![json!("Africa"), json!("Europe")],
        }];
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert!(
            compiled
                .sql
                .contains("(t0.\"Region\" NOT IN ('Africa', 'Europe') OR t0.\"Region\" IS NULL)"),
            "{}",
            compiled.sql
        );
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_region"),
            operation: FilterOperation::In,
            values: vec![json!("Africa"), json!(null)],
        }];
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert!(
            compiled
                .sql
                .contains("(t0.\"Region\" IS NULL OR t0.\"Region\" IN ('Africa'))"),
            "{}",
            compiled.sql
        );
    }

    #[test]
    fn len_filters_compare_the_length_of_the_field() {
        // `LENEQ`..`LENLTE` are `n.binary(op, n.func.LEN(f), val)`: the length of the field,
        // compared with an integer (`_mangle_array_filter` casts the value to `INTEGER`).
        for (operation, condition) in [
            (FilterOperation::Leneq, "LENGTH(t0.\"Region\") = 6"),
            (FilterOperation::Lenne, "LENGTH(t0.\"Region\") != 6"),
            (FilterOperation::Lengt, "LENGTH(t0.\"Region\") > 6"),
            (FilterOperation::Lengte, "LENGTH(t0.\"Region\") >= 6"),
            (FilterOperation::Lenlt, "LENGTH(t0.\"Region\") < 6"),
            (FilterOperation::Lenlte, "LENGTH(t0.\"Region\") <= 6"),
        ] {
            let mut spec = spec(vec![SelectItem {
                field_ref: FieldRef::id("f_region"),
                role: Role::Row,
                level: None,
            }]);
            spec.filters = vec![FilterSpec {
                field_ref: FieldRef::id("f_region"),
                operation,
                values: vec![json!(6)],
            }];
            let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
            assert!(
                compiled.sql.contains(condition),
                "{operation:?}: {}",
                compiled.sql
            );
        }
    }

    #[test]
    fn the_tree_role_slices_the_array_and_filters_by_its_length() {
        // `_make_tree_wrapper` + `_generate_tree_filter_items`, as the demo's "Table by
        // categories" sends them: `TREE(ARRAY(...))` selected as `slice(expr, 1, level)` and
        // filtered by `LEN(expr) >= level`.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_tree".to_string(),
            title: "Products".to_string(),
            initial_data_type: Some(UserDataType::TreeStr),
            data_type: Some(UserDataType::TreeStr),
            cast: Some(UserDataType::TreeStr),
            calc_mode: CalcMode::Formula,
            formula: "TREE(ARRAY([Region], [Region]))".to_string(),
            ..Field::default()
        });
        let spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_tree"),
            role: Role::Tree,
            level: Some(1),
        }]);
        let compiled = compile(&dataset, &spec, Dialect::Postgres).unwrap();
        // The Python reference (with its own aliasing) is
        // `(ARRAY[...])[1:(1 + 1) - 1]` plus `... LENGTH/ARRAY_LENGTH ... END >= 1`; the port
        // keeps one extra pair of parentheses around the translated formula and groups by the
        // same expression instead of by the alias, which is the same query.
        assert_eq!(
            compiled.sql,
            "SELECT ((ARRAY[t0.\"Region\", t0.\"Region\"]))[1:(1 + 1) - 1] AS \"f_tree\" \
             FROM \"public\".\"sales\" AS t0 \
             WHERE CASE WHEN ((ARRAY[t0.\"Region\", t0.\"Region\"]) IS NULL) \
             THEN CAST(NULL AS INTEGER) \
             ELSE coalesce(ARRAY_LENGTH((ARRAY[t0.\"Region\", t0.\"Region\"]), 1), 0) END >= 1 \
             GROUP BY ((ARRAY[t0.\"Region\", t0.\"Region\"]))[1:(1 + 1) - 1]"
        );
    }

    #[test]
    fn a_tree_level_is_locked_by_its_prefix() {
        // `_generate_tree_filter_items` sends `STARTSWITH(tree, prefix)` for a level nested in an
        // expanded branch, and `dl_formula` resolves it to `FuncStartswithArrayConst`: the leading
        // entries of the array are compared with the prefix itself.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_tree".to_string(),
            title: "Products".to_string(),
            initial_data_type: Some(UserDataType::TreeStr),
            data_type: Some(UserDataType::TreeStr),
            cast: Some(UserDataType::TreeStr),
            calc_mode: CalcMode::Formula,
            formula: "TREE(ARRAY([Region], [Region]))".to_string(),
            ..Field::default()
        });
        let mut spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_tree"),
            role: Role::Tree,
            level: Some(2),
        }]);
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_tree"),
            operation: FilterOperation::Startswith,
            values: vec![json!(["Europe"])],
        }];
        let compiled = compile(&dataset, &spec, Dialect::Postgres).unwrap();
        // `where` before `having`, and the `LENGTE` the tree item adds comes last: Python puts it
        // first, which is the same conjunction.
        assert!(
            compiled
                .sql
                .contains("[1:array_length(ARRAY['Europe'], 1)] = ARRAY['Europe']"),
            "{}",
            compiled.sql
        );
        // `LENGTE` still comes from the item's `level`.
        assert!(compiled.sql.contains("END >= 2"), "{}", compiled.sql);

        let clickhouse = compile(&dataset, &spec, Dialect::ClickHouse).unwrap();
        assert!(clickhouse.sql.contains("arraySlice("), "{}", clickhouse.sql);
        assert!(
            clickhouse.sql.contains("length(['Europe'])) = ['Europe']"),
            "{}",
            clickhouse.sql
        );
        // MySQL has no array type at all: the formula itself does not translate there, so the
        // `startswith` never gets a chance to fail on its own.
        assert!(compile(&dataset, &spec, Dialect::Mysql).is_err());
    }

    #[test]
    fn a_deep_tree_level_slices_beyond_the_probed_literals() {
        // A drill-down block carries the depth it expands in `level`, so a third nested branch
        // asks for `SLICE(TREE_STR, 1, 4)` — not one of the three literal offsets the registry
        // was probed with. `dl_formula` keeps the offset and the length as placeholders
        // (`(x)[{1}:({1} + {2}) - 1]`), so every depth translates; the port used to return
        // "no translation for SLICE(TREE_STR|lit:int:1|lit:int:4)" for it.
        let mut dataset = dataset();
        dataset.fields.push(Field {
            guid: "f_tree".to_string(),
            title: "Products".to_string(),
            initial_data_type: Some(UserDataType::TreeStr),
            data_type: Some(UserDataType::TreeStr),
            cast: Some(UserDataType::TreeStr),
            calc_mode: CalcMode::Formula,
            formula: "TREE(ARRAY([Region], [Region]))".to_string(),
            ..Field::default()
        });
        let spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_tree"),
            role: Role::Tree,
            level: Some(4),
        }]);
        let postgres = compile(&dataset, &spec, Dialect::Postgres).unwrap();
        assert!(postgres.sql.contains("[1:(1 + 4) - 1]"), "{}", postgres.sql);
        let clickhouse = compile(&dataset, &spec, Dialect::ClickHouse).unwrap();
        assert!(clickhouse.sql.contains("arraySlice("), "{}", clickhouse.sql);
        assert!(clickhouse.sql.contains(", 1, 4)"), "{}", clickhouse.sql);
    }

    #[test]
    fn an_unknown_field_is_reported_not_ignored() {
        let spec = spec(vec![SelectItem {
            field_ref: FieldRef::title("Nope"),
            role: Role::Row,
            level: None,
        }]);
        let error = compile(&dataset(), &spec, Dialect::Postgres).unwrap_err();
        assert!(matches!(error, QueryError::UnknownField(ref name) if name == "Nope"));
    }

    #[test]
    fn an_info_item_does_not_shift_order_by_positions() {
        let mut spec = spec(vec![
            SelectItem {
                field_ref: FieldRef::id("f_calc"),
                role: Role::Info,
                level: None,
            },
            SelectItem {
                field_ref: FieldRef::id("f_region"),
                role: Role::Row,
                level: None,
            },
        ]);
        spec.order_by = vec![OrderBySpec {
            field_ref: FieldRef::id("f_region"),
            direction: Direction::Asc,
        }];
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        // `f_region` is the first *selected* column, even though it is the second legend item.
        // `f_region` is a dimension, so the aggregated query groups by it.
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             GROUP BY t0.\"Region\" ORDER BY 1 ASC"
        );

        spec.order_by[0].field_ref = FieldRef::id("f_calc");
        let error = compile(&dataset(), &spec, Dialect::Postgres).unwrap_err();
        assert!(matches!(error, QueryError::NotSelected(ref name) if name == "f_calc"));
    }

    #[test]
    fn a_filter_over_a_missing_field_is_dropped_when_asked() {
        let mut spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_region"),
            role: Role::Row,
            level: None,
        }]);
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_gone"),
            operation: FilterOperation::Eq,
            values: vec![json!(1)],
        }];
        assert!(matches!(
            compile(&dataset(), &spec, Dialect::Postgres).unwrap_err(),
            QueryError::UnknownField(_)
        ));

        spec.ignore_nonexistent_filters = true;
        let compiled = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        // A selected dimension is grouped even when nothing aggregates.
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             GROUP BY t0.\"Region\""
        );
    }

    #[test]
    fn a_field_whose_formula_aggregates_gets_no_second_aggregation() {
        // `has_auto_aggregation`: Python does not wrap an already-aggregated formula in the
        // field's own aggregation — `SUM([Sales] + 1)` is not valid SQL.
        let mut dataset = dataset();
        let mut doubled = dataset.field_by_guid("f_calc").unwrap().clone();
        doubled.guid = "f_doubled".to_string();
        doubled.title = "Doubled".to_string();
        doubled.aggregation = Aggregation::Sum;
        dataset.fields.push(doubled);

        let compiled = compile(
            &dataset,
            &spec(vec![SelectItem {
                field_ref: FieldRef::id("f_doubled"),
                role: Role::Measure,
                level: None,
            }]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT (SUM(t0.\"Sales\") * 2) AS \"f_doubled\" FROM \"public\".\"sales\" AS t0"
        );
    }

    #[test]
    fn a_filter_over_an_aggregated_formula_becomes_having() {
        let mut dataset = dataset();
        let mut total = dataset.field_by_guid("f_calc").unwrap().clone();
        total.guid = "f_total".to_string();
        total.title = "Total".to_string();
        total.formula = "SUM([Sales])".to_string();
        total.guid_formula.clear();
        dataset.fields.push(total);

        let mut spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_region"),
            role: Role::Row,
            level: None,
        }]);
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_total"),
            operation: FilterOperation::Gt,
            values: vec![json!(10)],
        }];
        let compiled = compile(&dataset, &spec, Dialect::Postgres).unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             GROUP BY t0.\"Region\" HAVING (sum(t0.\"Sales\")) > 10"
        );
        assert!(compiled.grouped);
    }
}

/// `SELECT DISTINCT <field>` for `/values/distinct`.
///
/// `data-api` compiles the field expression exactly like in `/result` (formulas included),
/// wraps it in `DISTINCT` and applies the request's `where`; there is no aggregation, so no
/// `GROUP BY` either — which is why measure filters are rejected, like
/// `DistinctLegendFormalizer.validate_legend_item`.
pub fn compile_distinct(
    dataset: &Dataset,
    spec: &QuerySpec,
    dialect: Dialect,
) -> Result<CompiledQuery, QueryError> {
    let rls_spec = apply_rls(dataset, spec)?;
    let spec = rls_spec.as_ref().unwrap_or(spec);
    validate_query_fields(dataset, spec)?;
    let mut resolver = Resolver::new(dataset, spec, dialect)?;

    let field_ref = single_field(spec)?;
    let field = dataset
        .resolve(field_ref)
        .ok_or_else(|| unknown(field_ref))?;
    // `/values/distinct` has no `GROUP BY`, so the field is taken as it is *before* its own
    // aggregation: `SELECT DISTINCT t0."user_id"`, not `SELECT DISTINCT COUNT(...)`.
    let expr = resolver.casting(field)?;
    let where_parts = value_filters(dataset, &mut resolver, spec)?;
    let from = resolver.from_item()?;

    // `ValueDistinctSpecFormalizer.make_order_by_specs`: the values are sorted by the field
    // itself, so `values/distinct` is stable. Python orders by the select alias, and the
    // Postgres connector puts NULLs before every value (`force_nulls_lower_than_values`).
    let alias = dialect.quote_ident(&field.guid);
    let order_by = vec![format!("{alias} ASC{}", dialect.nulls_first_asc())];
    // The filters are compiled the same way a request's are, so they may have lifted leaves;
    // `/values/distinct` never forks (a LOD of a single value is meaningless), and the plan
    // puts every leaf back where it was.
    let plan = crate::fork::split(
        Select {
            items: vec![SelectExpr {
                sql: expr.sql,
                alias,
            }],
            from: Source::Table(from),
            joins: Vec::new(),
            filters: where_parts,
            group_by: Vec::new(),
            having: Vec::new(),
            order_by,
            limit: spec.limit.map(|value| value as usize),
            offset: spec.offset.map(|value| value as usize),
            ctes: Vec::new(),
        },
        &std::mem::take(&mut resolver.leaves),
        &[],
        dialect,
    )?;
    let sql = plan.render().replacen("SELECT ", "SELECT DISTINCT ", 1);

    Ok(CompiledQuery {
        sql,
        columns: vec![OutputColumn {
            legend_item_id: 0,
            guid: field.guid.clone(),
            title: field.title.clone(),
            data_type: expr.data_type,
            aggregation: Aggregation::None,
            // `DistinctLegendFormalizer` builds one item, with the `distinct` role.
            role: Role::Distinct,
            in_result_set: true,
        }],
        grouped: false,
    })
}

/// The single field a `/values/*` request is about.
fn single_field(spec: &QuerySpec) -> Result<&dl_core::FieldRef, QueryError> {
    spec.select
        .first()
        .map(|item| &item.field_ref)
        .ok_or_else(|| QueryError::Invalid("the request selects no fields".to_string()))
}

/// `WHERE` of a `/values/*` request: no aggregates there, so measure filters are an error.
fn value_filters(
    dataset: &Dataset,
    resolver: &mut Resolver<'_>,
    spec: &QuerySpec,
) -> Result<Vec<String>, QueryError> {
    let mut where_parts = Vec::with_capacity(spec.filters.len());
    for filter in &spec.filters {
        let Some(field) = dataset.resolve(&filter.field_ref) else {
            if spec.ignore_nonexistent_filters {
                continue;
            }
            return Err(unknown(&filter.field_ref));
        };
        if dataset.field_type(field) == FieldType::Measure {
            return Err(QueryError::MeasureFilterUnsupported {
                guid: field.guid.clone(),
            });
        }
        if let Some(part) = resolver.filter(filter, spec.ignore_nonexistent_filters)? {
            where_parts.push(part.condition);
        }
    }
    Ok(where_parts)
}

/// `SELECT MIN(x), MAX(x)` for `/values/range`.
pub fn compile_range(
    dataset: &Dataset,
    spec: &QuerySpec,
    dialect: Dialect,
) -> Result<CompiledQuery, QueryError> {
    let rls_spec = apply_rls(dataset, spec)?;
    let spec = rls_spec.as_ref().unwrap_or(spec);
    validate_query_fields(dataset, spec)?;
    let mut resolver = Resolver::new(dataset, spec, dialect)?;

    let field_ref = single_field(spec)?;
    let field = dataset
        .resolve(field_ref)
        .ok_or_else(|| unknown(field_ref))?;
    // `MIN`/`MAX` of the column, not of its aggregation (which would nest aggregates).
    let expr = resolver.casting(field)?;
    let dtype = expr.data_type;
    let where_parts = value_filters(dataset, &mut resolver, spec)?;
    let from = resolver.from_item()?;

    let min_alias = format!("{}_min", field.guid);
    let max_alias = format!("{}_max", field.guid);
    // `SingleRowQuerySpecFormalizerBase`: no `ORDER BY`, no `LIMIT`/`OFFSET` — the request
    // carries neither.
    //
    // The expression is a skeleton with `{leaf:N}` in it, so the plan is what puts the leaves
    // back: `MIN(date_trunc('month', {leaf:0}))` would otherwise reach the database as is.
    let plan = crate::fork::split(
        Select {
            items: vec![
                SelectExpr {
                    sql: format!("MIN({})", expr.sql),
                    alias: dialect.quote_ident(&min_alias),
                },
                SelectExpr {
                    sql: format!("MAX({})", expr.sql),
                    alias: dialect.quote_ident(&max_alias),
                },
            ],
            from: Source::Table(from),
            joins: Vec::new(),
            filters: where_parts,
            group_by: Vec::new(),
            having: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            ctes: Vec::new(),
        },
        &std::mem::take(&mut resolver.leaves),
        &[],
        dialect,
    )?;
    let sql = plan.render();

    Ok(CompiledQuery {
        sql,
        columns: vec![
            OutputColumn {
                legend_item_id: 0,
                // `RangeLegendFormalizer`: both items name the field itself, and `role_spec`
                // tells `min` from `max` — the response must not invent `<guid>_min` ids.
                guid: field.guid.clone(),
                title: field.title.clone(),
                data_type: dtype,
                aggregation: Aggregation::Min,
                role: Role::Range,
                in_result_set: true,
            },
            OutputColumn {
                legend_item_id: 1,
                guid: field.guid.clone(),
                title: field.title.clone(),
                data_type: dtype,
                aggregation: Aggregation::Max,
                role: Role::Range,
                in_result_set: true,
            },
        ],
        grouped: false,
    })
}

#[cfg(test)]
mod value_tests {
    use super::*;
    use crate::SelectItem;
    use dl_core::{Aggregation, Field, FieldRef, Source, UserDataType};
    use serde_json::json;

    fn dataset() -> Dataset {
        let mut region = Field {
            guid: "f_region".to_string(),
            title: "Region".to_string(),
            initial_data_type: Some(UserDataType::String),
            data_type: Some(UserDataType::String),
            ..Field::default()
        };
        region.source = "Region".to_string();
        let mut sales = region.clone();
        sales.guid = "f_sales".to_string();
        sales.title = "Sales".to_string();
        sales.initial_data_type = Some(UserDataType::Integer);
        sales.data_type = Some(UserDataType::Integer);
        sales.source = "Sales".to_string();
        sales.aggregation = Aggregation::Sum;
        Dataset {
            sources: vec![Source::table("src", "public.sales")],
            data_export_forbidden: false,
            connection_id: None,
            fields: vec![region, sales],
            ..Dataset::default()
        }
    }

    fn value_spec(guid: &str) -> QuerySpec {
        QuerySpec {
            select: vec![SelectItem {
                field_ref: FieldRef::id(guid),
                role: Role::Row,
                level: None,
            }],
            ..QuerySpec::default()
        }
    }

    #[test]
    fn distinct_is_select_distinct_ordered_by_the_field() {
        let mut spec = value_spec("f_region");
        spec.limit = Some(1000);
        spec.offset = Some(0);
        let compiled = compile_distinct(&dataset(), &spec, Dialect::Postgres).unwrap();
        // `ValueDistinctSpecFormalizer.make_order_by_specs`: `ORDER BY <the field> ASC`, and the
        // Postgres connector renders the NULLs placement explicitly.
        assert_eq!(
            compiled.sql,
            "SELECT DISTINCT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
             ORDER BY \"f_region\" ASC NULLS FIRST LIMIT 1000 OFFSET 0"
        );
        assert!(!compiled.grouped);
        assert_eq!(compiled.columns.len(), 1);
    }

    #[test]
    fn range_substitutes_the_leaves_of_a_formula() {
        let mut dataset = dataset();
        let mut doubled = dataset.field_by_guid("f_sales").unwrap().clone();
        doubled.guid = "f_doubled".to_string();
        doubled.title = "Doubled".to_string();
        doubled.calc_mode = CalcMode::Formula;
        doubled.formula = "[Sales] * 2".to_string();
        doubled.guid_formula.clear();
        doubled.aggregation = Aggregation::None;
        dataset.fields.push(doubled);

        let compiled =
            compile_range(&dataset, &value_spec("f_doubled"), Dialect::Postgres).unwrap();
        // The expression is a template over the plan's leaves: `MIN({leaf:0})` must not reach
        // the database as is.
        assert!(
            !compiled.sql.contains("{leaf:"),
            "unsubstituted leaf in {}",
            compiled.sql
        );
        assert_eq!(
            compiled.sql,
            "SELECT MIN((SUM(t0.\"Sales\") * 2)) AS \"f_doubled_min\", \
             MAX((SUM(t0.\"Sales\") * 2)) AS \"f_doubled_max\" \
             FROM \"public\".\"sales\" AS t0"
        );
    }

    #[test]
    fn range_selects_min_and_max_of_the_field() {
        let compiled =
            compile_range(&dataset(), &value_spec("f_sales"), Dialect::Postgres).unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT MIN(t0.\"Sales\") AS \"f_sales_min\", MAX(t0.\"Sales\") AS \"f_sales_max\" \
             FROM \"public\".\"sales\" AS t0"
        );
        assert_eq!(compiled.columns[0].aggregation, Aggregation::Min);
        assert_eq!(compiled.columns[1].aggregation, Aggregation::Max);
    }

    #[test]
    fn a_measure_filter_is_rejected_by_distinct_and_range() {
        let mut spec = value_spec("f_region");
        spec.filters = vec![FilterSpec {
            field_ref: FieldRef::id("f_sales"),
            operation: FilterOperation::Gt,
            values: vec![json!(10)],
        }];
        assert!(matches!(
            compile_distinct(&dataset(), &spec, Dialect::Postgres).unwrap_err(),
            QueryError::MeasureFilterUnsupported { ref guid } if guid == "f_sales"
        ));
        assert!(matches!(
            compile_range(&dataset(), &spec, Dialect::Postgres).unwrap_err(),
            QueryError::MeasureFilterUnsupported { .. }
        ));
    }
}

#[cfg(test)]
mod parameter_tests {
    use super::*;
    use crate::{ParameterValueSpec, SelectItem};
    use dl_core::{FieldRef, Source, UserDataType};
    use serde_json::json;

    fn dataset() -> Dataset {
        let sales = Field {
            guid: "f_sales".to_string(),
            title: "Sales".to_string(),
            initial_data_type: Some(UserDataType::Integer),
            data_type: Some(UserDataType::Integer),
            ..Field::default()
        };
        let parameter = Field {
            guid: "p_scale".to_string(),
            title: "Scale".to_string(),
            data_type: Some(UserDataType::Float),
            calc_mode: CalcMode::Parameter,
            default_value: Some(json!(0.5)),
            ..Field::default()
        };
        let scaled = Field {
            guid: "f_scaled".to_string(),
            title: "Scaled".to_string(),
            data_type: Some(UserDataType::Float),
            calc_mode: CalcMode::Formula,
            formula: "[Sales] * [Scale]".to_string(),
            ..Field::default()
        };
        Dataset {
            sources: vec![Source::table("src", "public.sales")],
            fields: vec![sales, parameter, scaled],
            ..Dataset::default()
        }
    }

    fn spec(select: Vec<SelectItem>) -> QuerySpec {
        QuerySpec {
            select,
            ..QuerySpec::default()
        }
    }

    #[test]
    fn a_parameter_uses_its_default_value_as_a_sql_literal() {
        let compiled = compile(
            &dataset(),
            &spec(vec![SelectItem {
                field_ref: FieldRef::id("p_scale"),
                role: Role::Row,
                level: None,
            }]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT 0.5 AS \"p_scale\" FROM \"public\".\"sales\" AS t0"
        );
        assert_eq!(compiled.columns[0].data_type.name(), "FLOAT");
    }

    #[test]
    fn parameter_values_override_the_default_inside_a_formula() {
        let mut spec = spec(vec![SelectItem {
            field_ref: FieldRef::id("f_scaled"),
            role: Role::Measure,
            level: None,
        }]);
        let default = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert!(
            default.sql.contains("0.5"),
            "the default value should be inlined: {}",
            default.sql
        );

        spec.parameter_values = vec![ParameterValueSpec {
            field_ref: FieldRef::title("Scale"),
            value: json!(3),
        }];
        let overridden = compile(&dataset(), &spec, Dialect::Postgres).unwrap();
        assert!(
            overridden.sql.contains("* 3)"),
            "the request value should win: {}",
            overridden.sql
        );
    }

    #[test]
    fn info_and_parameter_roles_are_legend_only() {
        let compiled = compile(
            &dataset(),
            &spec(vec![
                SelectItem {
                    field_ref: FieldRef::id("f_sales"),
                    role: Role::Measure,
                    level: None,
                },
                SelectItem {
                    field_ref: FieldRef::id("f_scaled"),
                    role: Role::Info,
                    level: None,
                },
                SelectItem {
                    field_ref: FieldRef::id("p_scale"),
                    role: Role::Parameter,
                    level: None,
                },
            ]),
            Dialect::Postgres,
        )
        .unwrap();
        // `f_sales` has no declared `type` in this dataset and does not aggregate, so it is a
        // dimension and the aggregated query groups by it (`make_group_by_specs` goes by the
        // field type, not by the role it was put in).
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"Sales\" AS \"f_sales\" FROM \"public\".\"sales\" AS t0 \
             GROUP BY t0.\"Sales\""
        );
        assert_eq!(compiled.columns.len(), 3);
        assert!(compiled.columns[0].in_result_set);
        assert!(!compiled.columns[1].in_result_set);
        assert!(!compiled.columns[2].in_result_set);
        assert_eq!(compiled.columns[2].role, Role::Parameter);
        assert_eq!(compiled.columns[2].legend_item_id, 2);
    }

    #[test]
    fn a_template_item_is_selected() {
        // `SELECTABLE_ROLES` in `dl_query_processing` is `FieldRole` minus
        // {info, order_by, filter, parameter}, so `template` *is* in the `SELECT` list.
        let compiled = compile(
            &dataset(),
            &spec(vec![
                SelectItem {
                    field_ref: FieldRef::id("f_sales"),
                    role: Role::Measure,
                    level: None,
                },
                SelectItem {
                    field_ref: FieldRef::id("f_scaled"),
                    role: Role::Template,
                    level: None,
                },
            ]),
            Dialect::Postgres,
        )
        .unwrap();
        assert!(
            compiled.sql.contains("AS \"f_scaled\""),
            "a template item is in the SELECT list: {}",
            compiled.sql
        );
        assert!(compiled.columns[1].in_result_set);
        assert_eq!(compiled.columns[1].role, Role::Template);
    }

    #[test]
    fn a_parameter_without_a_value_is_an_error() {
        let mut dataset = dataset();
        dataset.fields[1].default_value = None;
        let error = compile(
            &dataset,
            &spec(vec![SelectItem {
                field_ref: FieldRef::id("p_scale"),
                role: Role::Row,
                level: None,
            }]),
            Dialect::Postgres,
        )
        .unwrap_err();
        assert!(matches!(error, QueryError::MissingParameter { ref guid } if guid == "p_scale"));
    }

    #[tokio::test]
    async fn rls_adds_an_in_filter_for_the_authenticated_user() {
        use dl_core::{Rls, RlsEntry, RlsPattern, RlsSubject, RlsSubjectType};

        let mut dataset = Dataset {
            sources: vec![Source::table("src", "public.sales")],
            fields: vec![Field {
                guid: "f_region".to_string(),
                title: "Region".to_string(),
                initial_data_type: Some(UserDataType::String),
                data_type: Some(UserDataType::String),
                ..Field::default()
            }],
            ..Dataset::default()
        };
        dataset.rls = Rls {
            entries: vec![RlsEntry {
                field_guid: "f_region".to_string(),
                allowed_value: Some("Europe".to_string()),
                pattern_type: RlsPattern::Value,
                subject: RlsSubject {
                    subject_type: RlsSubjectType::User,
                    subject_id: "u1".to_string(),
                    subject_name: Some("u1".to_string()),
                },
            }],
        };
        let spec = spec(vec![SelectItem::new(FieldRef::id("f_region"), Role::Row)]);

        let sql = crate::identity::with_identity(
            crate::identity::Identity {
                user_id: "u1".to_string(),
                ..Default::default()
            },
            async { compile(&dataset, &spec, Dialect::Postgres).unwrap().sql },
        )
        .await;
        assert!(sql.contains(r#"WHERE t0."Region" IN ('Europe')"#), "{sql}");

        let other = crate::identity::with_identity(
            crate::identity::Identity {
                user_id: "u2".to_string(),
                ..Default::default()
            },
            async { compile(&dataset, &spec, Dialect::Postgres).unwrap().sql },
        )
        .await;
        assert!(other.contains("FALSE"), "{other}");

        assert!(matches!(
            compile(&dataset, &spec, Dialect::Postgres),
            Err(QueryError::Rls(_))
        ));
    }
}
/// Multi-source datasets: the `FROM` clause the avatar relations produce.
#[cfg(test)]
mod multisource_tests {
    use super::*;
    use crate::SelectItem;
    use dl_core::{
        AvatarRelation, Field, FieldRef, JoinCondition, JoinConditionPart, JoinType, Source,
        UserDataType,
    };

    /// A plain string dimension of one avatar, aggregated by nothing.
    fn field(guid: &str, title: &str, source: &str, avatar: &str) -> Field {
        let mut field = Field {
            guid: guid.to_string(),
            title: title.to_string(),
            initial_data_type: Some(UserDataType::String),
            data_type: Some(UserDataType::String),
            source: source.to_string(),
            avatar_id: Some(avatar.to_string()),
            ..Field::default()
        };
        field.aggregation = Aggregation::None;
        field
    }

    /// Two avatars (`orders`, `managers`) joined on the manager's category column, i.e. the shape
    /// the demo dataset has.
    fn dataset() -> Dataset {
        Dataset {
            sources: vec![
                Source::table("avatar-orders", "public.orders"),
                Source::table("avatar-managers", "public.managers"),
            ],
            fields: vec![
                field("f_category", "Category", "category", "avatar-orders"),
                field("f_manager", "Manager", "manager", "avatar-managers"),
            ],
            avatar_relations: vec![AvatarRelation {
                id: "relation".to_string(),
                left_avatar_id: "avatar-orders".to_string(),
                right_avatar_id: "avatar-managers".to_string(),
                join_type: JoinType::Inner,
                conditions: vec![JoinCondition {
                    left: JoinConditionPart::Direct("category".to_string()),
                    right: JoinConditionPart::Direct("category".to_string()),
                    operator: "eq".to_string(),
                }],
            }],
            root_avatar_id: Some("avatar-orders".to_string()),
            ..Dataset::default()
        }
    }

    fn item(guid: &str) -> SelectItem {
        SelectItem {
            field_ref: FieldRef::id(guid),
            role: Role::Row,
            level: None,
        }
    }

    #[test]
    fn a_second_source_is_joined_and_aliased() {
        let compiled = compile(
            &dataset(),
            &spec(vec![item("f_manager")]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t1.\"manager\" AS \"f_manager\" FROM \"public\".\"orders\" AS t0 \
             INNER JOIN \"public\".\"managers\" AS t1 ON t0.\"category\" = t1.\"category\" \
             GROUP BY t1.\"manager\""
        );
    }

    #[test]
    fn a_query_over_one_source_does_not_join_the_other() {
        let compiled = compile(
            &dataset(),
            &spec(vec![item("f_category")]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"category\" AS \"f_category\" FROM \"public\".\"orders\" AS t0 \
             GROUP BY t0.\"category\""
        );
    }

    #[test]
    fn a_left_relation_keeps_the_rows_of_the_root() {
        let mut dataset = dataset();
        dataset.avatar_relations[0].join_type = JoinType::Left;
        let compiled =
            compile(&dataset, &spec(vec![item("f_manager")]), Dialect::Postgres).unwrap();
        assert!(compiled.sql.contains("LEFT JOIN"), "{}", compiled.sql);
    }

    #[test]
    fn sources_no_relation_joins_are_an_error() {
        let mut dataset = dataset();
        dataset.avatar_relations.clear();
        let error =
            compile(&dataset, &spec(vec![item("f_manager")]), Dialect::Postgres).unwrap_err();
        assert!(matches!(error, QueryError::MultipleSources(2)));
    }

    #[test]
    fn two_avatars_over_one_table_are_a_self_join() {
        // The `orders` → `orders` shape: the very same table twice, joined on one of its columns.
        // Python aliases each avatar separately, so the two read different rows of one table.
        let mut dataset = dataset();
        dataset
            .sources
            .insert(1, Source::table("avatar-orders-previous", "public.orders"));
        dataset.fields.push(field(
            "f_previous_category",
            "Previous category",
            "category",
            "avatar-orders-previous",
        ));
        dataset.avatar_relations.push(AvatarRelation {
            id: "self".to_string(),
            left_avatar_id: "avatar-orders".to_string(),
            right_avatar_id: "avatar-orders-previous".to_string(),
            join_type: JoinType::Left,
            conditions: vec![JoinCondition {
                left: JoinConditionPart::Direct("category".to_string()),
                right: JoinConditionPart::Direct("category".to_string()),
                operator: "eq".to_string(),
            }],
        });
        let compiled = compile(
            &dataset,
            &spec(vec![item("f_category"), item("f_previous_category")]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"category\" AS \"f_category\", t1.\"category\" AS \"f_previous_category\" \
             FROM \"public\".\"orders\" AS t0 \
             LEFT JOIN \"public\".\"orders\" AS t1 ON t0.\"category\" = t1.\"category\" \
             GROUP BY t0.\"category\", t1.\"category\""
        );
    }

    #[test]
    fn an_unrelated_avatar_of_the_root_table_shares_its_alias() {
        // A second avatar over the root's table that no relation mentions is the same `FROM`
        // item: reading its field cannot turn into a cartesian product.
        let mut dataset = dataset();
        dataset
            .sources
            .push(Source::table("avatar-orders-copy", "public.orders"));
        dataset.fields.push(field(
            "f_copy_category",
            "Copied category",
            "category",
            "avatar-orders-copy",
        ));
        let compiled = compile(
            &dataset,
            &spec(vec![item("f_copy_category")]),
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "SELECT t0.\"category\" AS \"f_copy_category\" FROM \"public\".\"orders\" AS t0 \
             GROUP BY t0.\"category\""
        );
    }

    fn spec(select: Vec<SelectItem>) -> QuerySpec {
        QuerySpec {
            select,
            ..QuerySpec::default()
        }
    }
}
