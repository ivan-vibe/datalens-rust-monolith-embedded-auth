//! SQL generation driven by the template registry extracted from `dl_formula`.
//!
//! Pipeline: parse -> infer the data type of every node -> find the template for
//! `(dialect, function, argument types)` -> substitute the rendered arguments.
//! Anything the registry cannot answer is reported as unsupported so the caller can
//! fall back to the Python implementation instead of emitting wrong SQL.

use std::cell::RefCell;

use crate::ast::{
    BinOp, Clauses, FieldName, FieldNames, FieldTypes, Grouping, IsKind, Item, LodKind,
    OrderingDirection, UnOp,
};
use crate::dtype::DataType;
use crate::func_props;
use crate::registry;

const PREC_ATOM: u8 = 7;
/// `NOT` binds tighter than `AND`/`OR` but looser than a comparison: SQLAlchemy prints
/// `a = 1 AND NOT b` and `NOT (NOT b)`.
const PREC_NOT: u8 = 3;
const PREC_NEG: u8 = 6;

/// `COMPARABLE_TYPES` of `dl_formula.definitions.operators_binary`: the pairs of types `=` and
/// `IN` accept. A pair matches when both sides `casts_to` theirs, so `INTEGER` compares with
/// `FLOAT` and `TREE_STR` with `ARRAY_STR`, while `DATETIMETZ` and `MARKUP` compare with
/// nothing (they are in no pair) and are reported unsupported.
const COMPARABLE_TYPES: &[(DataType, DataType)] = &[
    (DataType::Boolean, DataType::Boolean),
    (DataType::String, DataType::String),
    (DataType::Float, DataType::Float),
    (DataType::Date, DataType::Date),
    (DataType::Datetime, DataType::Datetime),
    (DataType::Date, DataType::Datetime),
    (DataType::Datetime, DataType::Date),
    (DataType::GenericDatetime, DataType::GenericDatetime),
    (DataType::Date, DataType::GenericDatetime),
    (DataType::GenericDatetime, DataType::Date),
    (DataType::Geopoint, DataType::Geopoint),
    (DataType::Geopolygon, DataType::Geopolygon),
    (DataType::Uuid, DataType::Uuid),
    (DataType::ArrayInt, DataType::ArrayInt),
    (DataType::ArrayFloat, DataType::ArrayFloat),
    (DataType::ArrayStr, DataType::ArrayStr),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Mysql,
    ClickHouse,
}

impl Dialect {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Some(Dialect::Postgres),
            "mysql" => Some(Dialect::Mysql),
            "clickhouse" => Some(Dialect::ClickHouse),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Mysql => "mysql",
            Dialect::ClickHouse => "clickhouse",
        }
    }

    fn quote_char(self) -> char {
        match self {
            // SQLAlchemy's PostgreSQL and ClickHouse dialects both quote with `"`, and
            // only MySQL uses backticks. The native output has to match the SQL the
            // Python engine produces character for character, so follow SQLAlchemy here.
            Dialect::Postgres | Dialect::ClickHouse => '"',
            Dialect::Mysql => '`',
        }
    }
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub sql: String,
    pub data_type: DataType,
}

struct Rendered {
    sql: String,
    dtype: DataType,
    prec: u8,
    /// Registry key part: `lit:str:day`, `lit:int:7`, ... or the type name.
    token: String,
}

pub struct Codegen<'a> {
    dialect: Dialect,
    types: &'a FieldTypes,
    names: &'a FieldNames,
    /// Type inference only: the SQL of a formula with a clause the translator does not emit
    /// (`SUM(x) TOTAL`, `AGO(...)`) is not needed to know its type, and `dl_formula` computes
    /// the type from the same registry entries the SQL comes from. `infer_type` sets this;
    /// [`compile`] never does.
    types_only: bool,
    /// Calls that cannot be put inline and must be computed in a sub-query, in the order they
    /// were met while rendering. See [`ExternalCall`].
    externals: Option<RefCell<Vec<ExternalCall>>>,
    /// Leaf mode: every column and every aggregate call becomes `{leaf:N}` and is reported in
    /// [`CompiledLeaves`], so a query plan can decide which of them go into the base query and
    /// which into a fork. See [`compile_leaves`].
    leaves: Option<RefCell<Vec<Leaf>>>,
    /// While rendering the arguments of an aggregate, nested references belong to that
    /// aggregate's own SQL: `count(DISTINCT {leaf:0})` would be a leaf inside a leaf.
    inside_aggregate: std::cell::Cell<bool>,
    /// The LOD clause of the call being rendered; [`Codegen::leaf`] attaches it to the leaf.
    pending_external: RefCell<Option<ExternalCall>>,
    /// The number of leaves a plan has already allocated: see [`compile_leaves_from`].
    leaf_start: usize,
    /// The request a window function belongs to: see [`WindowContext`].
    window: &'a WindowContext,
}

/// A leaf of a formula: a column or an aggregate call, i.e. something a `SELECT` can name.
///
/// `dl_query_processing` splits a formula at these points: the base query selects the
/// non-forked leaves, a `QueryFork` computes the forked ones, and the formula itself becomes an
/// expression over the two (`CAST(q_0.e_1 AS DOUBLE PRECISION)/q_1.e_3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// The leaf rendered over the base source, e.g. `count(DISTINCT t0."user_id")`.
    pub sql: String,
    pub data_type: DataType,
    /// An aggregate or window call — it cannot be put into `GROUP BY` and it forces grouping.
    pub aggregated: bool,
    /// Set when the call carries a LOD clause: the query layer has to fork it out.
    pub external: Option<ExternalCall>,
    /// The leaf *is* a window call (`MAX(x) OVER (...)`, also when it was forked).
    ///
    /// A window belongs to the query that groups the rows, never to a query of its own: unlike an
    /// aggregate, it can be computed above a fork and it can be an item of the base query — and a
    /// window built from another one is a fork of its own
    /// ([`ExternalKind::Window`], [`Codegen::hand_off_window_argument`]).
    pub window: bool,
}

/// A formula split into its leaves, with `{leaf:N}` in the SQL where each leaf was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledLeaves {
    pub sql: String,
    pub data_type: DataType,
    pub leaves: Vec<Leaf>,
}

/// A call that `dl_query_processing` lifts into a sub-query, with the placeholder left in the
/// formula's SQL.
///
/// `ExtAggregationToQueryForkMutation` turns `COUNTD([user] FIXED [dim])` into a `QueryFork`
/// whose `result_expr` is the bare `COUNTD([user])` grouped by `[dim]`; the splitter then puts
/// that expression in its own sub-query and joins it back, so the outer formula only sees a
/// column of the joined result. This struct is that hand-off: [`ExternalCall::sql`] is what the
/// sub-query selects, [`ExternalCall::dimensions`] is what it groups by, and the rendered
/// formula carries `{ext:N}` where the joined column goes.
/// Why a call left the base query.
///
/// `dl_query_processing` forks three things, and they differ only in what the forked
/// sub-query groups by and which filters it may apply:
/// - a LOD aggregation (`COUNTD([a] FIXED [b])`) groups by its own dimensions;
/// - `SUM([a] TOTAL)` groups by nothing — it is one row for the whole source;
/// - a window with `BEFORE FILTER BY` groups by its `WITHIN` partition but must not apply the
///   request filters on the `BEFORE FILTER BY` dimensions, because it sees the rows before
///   they were filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalKind {
    Lod(LodKind),
    Total,
    /// `SUM([a] BEFORE FILTER BY [b])`, and the argument of a window that is built from another
    /// window (`MAX(COUNT([a]) AMONG [d])`): `WinFuncQuerySplitter` keeps a window function as
    /// the topmost node of its query, so the window below it is a query of its own, grouped by
    /// the request's dimensions, and the call reads the column it selects.
    Window,
    /// `AGO([x], [date], 'month', 1)`: not an aggregation at all — `dl_formula` replaces it
    /// with the measure itself and joins the same query shifted along the date dimension
    /// (`LookupFunctionToQueryForkMutation`).
    Lookup,
}

/// What the join of a lookup fork compares.
///
/// `AGO([Sales], [date], 'month', 1)` joins the base query to a copy of itself on
/// `base.date = fork.date + INTERVAL '1 MONTH'`, so the fork's rows are the previous month's.
/// `AT_DATE([Sales], [date], #2019-01-01#)` joins on `base.<the date argument> = fork.date`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// The lookup dimension, as rendered SQL.
    pub dimension: String,
    /// The base side of the join, with the dimension's column left as the marker `{dim}`.
    ///
    /// `AGO` compares the base's own dimension (`{dim}`); `AT_DATE` compares the date
    /// expression it was given, which is why this is a string and not a flag.
    pub base: String,
    /// The fork side of the join, with the fork dimension left as the marker `{dim}`.
    ///
    /// `AGO`: `DATEADD({dim}, unit, number)`. `AT_DATE`: the dimension itself (`{dim}`).
    pub offset: String,
    /// `DAY(dimension)`, compared as well for month-based units so that a 31-day month does
    /// not match the 28-day month a month earlier.
    pub day: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCall {
    /// The call rendered over the base source, e.g. `count(DISTINCT t0."user_id")`.
    pub sql: String,
    /// Grouping dimensions as rendered SQL, and the LOD kind that produced them.
    pub dimensions: Vec<String>,
    pub kind: ExternalKind,
    pub data_type: DataType,
    /// `BEFORE FILTER BY` dimensions as rendered SQL: the request filters that mention them
    /// stay outside the fork.
    pub skip_filters: Vec<String>,
    /// Set for a lookup: what the fork is joined on.
    pub lookup: Option<Lookup>,
}

/// What a window function needs to know about the request it is part of.
///
/// `dl_formula` resolves both before it renders anything: `FormulaCompiler.apply_mutations`
/// passes the query's group-by fields to `AmongToWithinGroupingMutation` and the request's
/// `ORDER BY` to `DefaultWindowOrderingMutation`. Without them `AMONG` cannot be translated
/// at all and `RSUM` has no ordering, i.e. sums its whole partition instead of running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowContext {
    /// The request's dimensions, as rendered SQL and in field order.
    pub group_dims: Vec<String>,
    /// The request's `ORDER BY`, as rendered SQL with a `DESC` flag.
    pub ordering: Vec<(String, bool)>,
}

/// Compile a parsed formula to SQL for the given dialect.
pub fn compile(item: &Item, dialect: Dialect, types: &FieldTypes) -> Result<Compiled, String> {
    let empty = FieldNames::new();
    compile_named(item, dialect, types, &empty)
}

/// The type of a parsed formula, without asking for its SQL.
///
/// `FormulaCompiler.get_field_final_data_type` is a type computation, not a translation: a
/// formula the *query* layer lifts into a sub-query (`SUM([x] TOTAL)`, a lookup) still has a
/// well-defined type, and Python reports it in the legend. Rendering the SQL first would make
/// those fields look broken.
pub fn infer_type(item: &Item, dialect: Dialect, types: &FieldTypes) -> Result<DataType, String> {
    let empty = FieldNames::new();
    let gen = Codegen {
        dialect,
        types,
        names: &empty,
        types_only: true,
        externals: None,
        leaves: None,
        inside_aggregate: std::cell::Cell::new(false),
        pending_external: RefCell::new(None),
        leaf_start: 0,
        window: &WindowContext::default(),
    };
    gen.render(item, 0).map(|rendered| rendered.dtype)
}

/// Split a formula into the leaves a query plan can select, keeping `{leaf:N}` in its SQL.
///
/// This is the entry point of the fork layer: `COUNTD([user] FIXED [dim])` comes back as one
/// leaf marked `external` and nothing else, while `CAST(COUNTD([user]) AS float)/COUNTD([user]
/// FIXED)` comes back as a template over three leaves — the two aggregates (`q_0.e_1`,
/// `q_1.e_3` once placed) and the division that stays in the outer query.
pub fn compile_leaves(
    item: &Item,
    dialect: Dialect,
    types: &FieldTypes,
    names: &FieldNames,
) -> Result<CompiledLeaves, String> {
    compile_leaves_from(item, dialect, types, names, 0, &WindowContext::default())
}

/// [`compile_leaves`], numbering the reported leaves from `start`.
///
/// A query selects several formulas: their leaves share one plan, so a formula compiled after
/// another must not reuse `{leaf:0}`. A substituted reference (`[Orders]` -> `countd(t0.id)`)
/// already carries the outer numbering, which is why the offset is explicit. `window` is what
/// the request gives every window function in it: its dimensions (`AMONG`) and its `ORDER BY`.
pub fn compile_leaves_from(
    item: &Item,
    dialect: Dialect,
    types: &FieldTypes,
    names: &FieldNames,
    start: usize,
    window: &WindowContext,
) -> Result<CompiledLeaves, String> {
    let gen = Codegen {
        dialect,
        types,
        names,
        types_only: false,
        externals: None,
        leaves: Some(RefCell::new(Vec::new())),
        inside_aggregate: std::cell::Cell::new(false),
        pending_external: RefCell::new(None),
        leaf_start: start,
        window,
    };
    let rendered = gen.render(item, 0)?;
    Ok(CompiledLeaves {
        sql: rendered.sql,
        data_type: rendered.dtype,
        leaves: gen.leaves.map(RefCell::into_inner).unwrap_or_default(),
    })
}

/// Compile a formula, reporting the calls the query layer has to fork instead of failing on
/// them.
///
/// This is [`compile`] plus the `QueryFork` hand-off: an aggregation with a LOD specifier is
/// rendered over the base source and returned in the list, and its place in the SQL becomes
/// `{ext:N}` (see [`ExternalCall`]). A formula with no such call returns an empty list and the
/// same SQL [`compile`] would produce.
pub fn compile_external(
    item: &Item,
    dialect: Dialect,
    types: &FieldTypes,
    names: &FieldNames,
) -> Result<(Compiled, Vec<ExternalCall>), String> {
    let gen = Codegen {
        dialect,
        types,
        names,
        types_only: false,
        externals: Some(RefCell::new(Vec::new())),
        leaves: None,
        inside_aggregate: std::cell::Cell::new(false),
        pending_external: RefCell::new(None),
        leaf_start: 0,
        window: &WindowContext::default(),
    };
    let rendered = gen.render(item, 0)?;
    Ok((
        Compiled {
            sql: rendered.sql,
            data_type: rendered.dtype,
        },
        gen.externals.map(RefCell::into_inner).unwrap_or_default(),
    ))
}

pub fn compile_named(
    item: &Item,
    dialect: Dialect,
    types: &FieldTypes,
    names: &FieldNames,
) -> Result<Compiled, String> {
    let gen = Codegen {
        dialect,
        types,
        names,
        types_only: false,
        externals: None,
        leaves: None,
        inside_aggregate: std::cell::Cell::new(false),
        pending_external: RefCell::new(None),
        leaf_start: 0,
        window: &WindowContext::default(),
    };
    let rendered = gen.render(item, 0)?;
    Ok(Compiled {
        sql: rendered.sql,
        data_type: rendered.dtype,
    })
}

impl<'a> Codegen<'a> {
    fn render(&self, item: &Item, min_prec: u8) -> Result<Rendered, String> {
        let mut rendered = self.render_inner(item)?;
        if rendered.prec < min_prec {
            rendered.sql = format!("({})", rendered.sql);
            rendered.prec = PREC_ATOM;
        }
        Ok(rendered)
    }

    fn render_inner(&self, item: &Item) -> Result<Rendered, String> {
        match item {
            Item::Field(name) => {
                let dtype = self.field_type(name);
                let rendered = Rendered {
                    sql: self.render_field(name),
                    dtype,
                    prec: PREC_ATOM,
                    token: dtype.name().to_string(),
                };
                // A column is a leaf of the plan: the base query selects it, the formula above
                // refers to it as `q_0.e_N`. A constant is not — it needs no column.
                Ok(self.leaf(rendered, false, false, None))
            }
            Item::Num { text, is_int } => {
                let dtype = if *is_int {
                    DataType::ConstInteger
                } else {
                    DataType::ConstFloat
                };
                // The registry token and the SQL literal are the *same* rendering: `dl_formula`
                // builds `lit:int:<int>` / `lit:float:<repr(float)>` when the registry is
                // generated and prints the value with the very same `int`/`repr` in the SQL.
                let literal = format_number(text, *is_int);
                let token = if *is_int {
                    format!("lit:int:{literal}")
                } else {
                    format!("lit:float:{literal}")
                };
                Ok(Rendered {
                    sql: literal,
                    dtype,
                    prec: PREC_ATOM,
                    token,
                })
            }
            Item::Str(text) => Ok(Rendered {
                // `dl_formula` doubles a percent sign for SQLAlchemy's pyformat paramstyle, and
                // SQLAlchemy undoes it when the statement is compiled: a literal `'%'` reaches
                // the database as one sign. The port builds the statement itself, so it is
                // written the way the database reads it — `STR(x) + "%"` is `|| '%'`.
                sql: format!("'{}'", text.replace('\'', "''")),
                dtype: DataType::ConstString,
                prec: PREC_ATOM,
                token: format!("lit:str:{text}"),
            }),
            // A date literal is a bind parameter in `dl_formula` (the reference literalizer
            // leaves the default `sa.literal`), and this engine builds one statement instead,
            // so it is written as the typed literal `dl_query` already uses for filter values:
            // `CAST('...' AS DATE)` on PostgreSQL/MySQL, `toDate32`/`toDateTime` on ClickHouse.
            Item::Date { text, is_datetime } => {
                let dtype = if *is_datetime {
                    DataType::ConstDatetime
                } else {
                    DataType::ConstDate
                };
                let quoted = format!("'{}'", text.replace('\'', "''"));
                let sql = match (self.dialect, *is_datetime) {
                    (Dialect::ClickHouse, true) => format!("toDateTime({quoted})"),
                    (Dialect::ClickHouse, false) => format!("toDate32({quoted})"),
                    (_, true) => format!("CAST({quoted} AS TIMESTAMP)"),
                    (_, false) => format!("CAST({quoted} AS DATE)"),
                };
                Ok(Rendered {
                    sql,
                    dtype,
                    prec: PREC_ATOM,
                    token: dtype.name().to_string(),
                })
            }
            Item::Bool(value) => {
                let sql = match (self.dialect, *value) {
                    (Dialect::ClickHouse, true) => "1",
                    (Dialect::ClickHouse, false) => "0",
                    (_, true) => "true",
                    (_, false) => "false",
                };
                Ok(Rendered {
                    sql: sql.to_string(),
                    dtype: DataType::ConstBoolean,
                    prec: PREC_ATOM,
                    token: format!("lit:bool:{value}"),
                })
            }
            Item::Null => Ok(Rendered {
                sql: "NULL".to_string(),
                dtype: DataType::Null,
                prec: PREC_ATOM,
                token: "NULL".to_string(),
            }),
            Item::Unary(op, inner) => self.render_unary(*op, inner),
            Item::Binary(op, left, right) => self.render_binary(*op, left, right),
            Item::Case {
                operand,
                branches,
                else_,
            } => self.render_case(operand.as_deref(), branches, else_.as_deref()),
            Item::Between {
                value,
                low,
                high,
                negated,
            } => self.render_between(value, low, high, *negated),
            Item::Is {
                value,
                kind,
                negated,
            } => self.render_is(value, *kind, *negated),
            Item::In {
                value,
                list,
                negated,
            } => self.render_in(value, list, *negated),
            Item::Call {
                name,
                args,
                clauses,
            } => {
                let is_window =
                    func_props::is_window_call(name, clauses.grouping != Grouping::Default);
                // A LOD clause does not make the call any less of an aggregate: it is
                // `count(DISTINCT ...)` either way, only computed in another query.
                let aggregated = is_window || func_props::can_be_aggregate(name);
                // Arguments belong to the call's own SQL, not to the plan.
                let was_inside = self.inside_aggregate.replace(aggregated);
                let rendered = self.render_clause_call(name, args, clauses)?;
                self.inside_aggregate.set(was_inside);
                // An aggregate or window call is a leaf too: `SUM(x)` in the base query is a
                // column of its result (`q_0.e_N`), never an inline expression of the outer
                // formula once the formula needs a fork.
                let external = self.pending_external.borrow_mut().take();
                if !aggregated {
                    // A plain call (`CAST`, `ARRAY`, `UPPER`) is not a column of the plan: it
                    // stays in the formula, and only its arguments may be leaves.
                    return Ok(rendered);
                }
                Ok(self.leaf(rendered, aggregated, is_window, external))
            }
        }
    }

    /// Report a leaf and put `{leaf:N}` in its place, when the plan asked for the split.
    ///
    /// Outside leaf mode this is the identity, so [`compile`] keeps producing one flat
    /// expression.
    fn leaf(
        &self,
        rendered: Rendered,
        aggregated: bool,
        window: bool,
        external: Option<ExternalCall>,
    ) -> Rendered {
        let Some(sink) = &self.leaves else {
            return rendered;
        };
        if self.inside_aggregate.get() {
            return rendered;
        }
        // A substituted reference may already be a leaf of the plan (`[Sales]` -> `{leaf:0}`):
        // lifting it again would put a placeholder inside a placeholder.
        if rendered.sql.starts_with("{leaf:") {
            return rendered;
        }
        let index = {
            let mut leaves = sink.borrow_mut();
            leaves.push(Leaf {
                sql: rendered.sql.clone(),
                data_type: rendered.dtype,
                aggregated,
                external,
                window,
            });
            self.leaf_start + leaves.len() - 1
        };
        Rendered {
            sql: format!("{{leaf:{index}}}"),
            dtype: rendered.dtype,
            prec: PREC_ATOM,
            token: rendered.token,
        }
    }

    /// Hand a call that another query has to compute to the caller.
    ///
    /// With a plan the call becomes a leaf and the caller attaches the clause; without one it
    /// becomes `{ext:N}` of [`compile_external`], which is what `dl_query` used before the
    /// plan layer existed.
    fn hand_off(&self, external: ExternalCall, call: Rendered) -> Rendered {
        // A forked call is a query of its own, so it becomes a leaf right here — also when an
        // aggregate is around it: `AVG(COUNTD([user] INCLUDE [date]))` averages the rows the
        // fork produced, and the `AVG` is a leaf of its own over that placeholder.
        self.hand_off_forced(external, call)
    }

    /// [`Codegen::hand_off`] for a call whose own SQL is already a leaf's placeholder.
    ///
    /// [`Codegen::leaf`] refuses to lift a placeholder again, which is right for a reference to
    /// an aggregate already in the plan. A lookup is different: `AGO([Sales], [date])` *is*
    /// `[Sales]`, but the plan has to compute it in a query of its own, so the leaf has to be
    /// created even though its SQL is the placeholder of the measure's leaf.
    fn hand_off_forced(&self, external: ExternalCall, call: Rendered) -> Rendered {
        let Some(sink) = &self.leaves else {
            return self.hand_off_external(external, call);
        };
        let index = {
            let mut leaves = sink.borrow_mut();
            leaves.push(Leaf {
                sql: call.sql.clone(),
                data_type: call.dtype,
                // A lookup's result is the aggregate it looks up: `AGO` only takes measures.
                aggregated: true,
                external: Some(external),
                // A fork is a query of its own, and its *result* is a column, not a window.
                window: false,
            });
            self.leaf_start + leaves.len() - 1
        };
        Rendered {
            sql: format!("{{leaf:{index}}}"),
            dtype: call.dtype,
            prec: PREC_ATOM,
            token: call.token,
        }
    }

    /// Hand a window's argument to a query of its own, when the argument contains a window.
    ///
    /// `WinFuncQuerySplitter` leaves a window function as the topmost node of its query, so a
    /// window the argument is built from cannot be part of the same `SELECT`:
    /// `MAX(COUNT(x) AMONG [d])` is `max(count(x)) OVER (PARTITION BY …)`, one window, while
    /// `MAX(CASE WHEN … max(count(x)) OVER (…) … END)` is a window inside a window, which no
    /// dialect accepts (`window function calls cannot be nested`, SQLSTATE 42P20). The argument
    /// becomes a fork grouped by the request's dimensions — exactly the rows the outer window
    /// sees — and the call reads the column it selects.
    fn hand_off_window_argument(&self, argument: Rendered) -> Rendered {
        // Without a plan there is no fork to hand the argument to, and a type query renders the
        // fields unsubstituted (`infer_type`), so neither can split anything off.
        if self.types_only {
            return argument;
        }
        let Some(sink) = &self.leaves else {
            return argument;
        };
        let mut external = ExternalCall {
            sql: argument.sql.clone(),
            dimensions: self.window.group_dims.clone(),
            kind: ExternalKind::Window,
            data_type: argument.dtype,
            skip_filters: Vec::new(),
            lookup: None,
        };
        // A reference to another field is one leaf of the plan (`{leaf:4}`, the `CASE` that
        // field's formula rendered, window and all): the leaf itself becomes the fork, so the
        // expression is computed once and every window over it reads the same column.
        if let Some(index) = single_leaf(&argument.sql) {
            let Some(local) = index.checked_sub(self.leaf_start) else {
                return argument;
            };
            let mut leaves = sink.borrow_mut();
            let Some(leaf) = leaves.get_mut(local) else {
                return argument;
            };
            if leaf.external.is_none() {
                // The fork selects the leaf's own expression, not the placeholder that names it.
                external.sql = leaf.sql.clone();
                leaf.external = Some(external);
            }
            return argument;
        }
        // The same argument is often the input of two windows (`[precalc]/MAX([precalc] AMONG
        // [d])`): one fork computes it, and both windows read its column.
        let existing = sink
            .borrow()
            .iter()
            .position(|leaf| leaf.external.as_ref() == Some(&external));
        if let Some(position) = existing {
            return Rendered {
                sql: format!("{{leaf:{}}}", self.leaf_start + position),
                ..argument
            };
        }
        self.hand_off_forced(external, argument)
    }

    /// Render the arguments of a call, forking off an argument that contains a window.
    ///
    /// `window` tells whether the call itself is a window function: only then does a window
    /// inside an argument have to move down a level — `ROUND(RSUM(x))` stays one query, because
    /// the window is inside the round, not inside another window.
    ///
    /// The window an argument is built from is either a node of the tree or a *leaf* of the plan:
    /// a reference to another field is substituted with the leaf that field's formula left
    /// (`[precalc]` is `{leaf:4}`), and that leaf's own SQL carries the window.
    fn render_args(&self, args: &[Item], window: bool) -> Result<Vec<Rendered>, String> {
        let mut rendered: Vec<Rendered> = Vec::with_capacity(args.len());
        for arg in args {
            let mut argument = self.render(arg, 0)?;
            if window && self.has_window(arg, &argument) {
                argument = self.hand_off_window_argument(argument);
            }
            rendered.push(argument);
        }
        Ok(rendered)
    }

    /// Whether an argument of a window function computes a window of its own.
    fn has_window(&self, item: &Item, argument: &Rendered) -> bool {
        contains_window(item)
            || is_window_sql(&argument.sql)
            || self.mentions_window_leaf(&argument.sql)
    }

    /// Whether a template names a leaf that is a window or carries one.
    fn mentions_window_leaf(&self, template: &str) -> bool {
        let Some(sink) = &self.leaves else {
            return false;
        };
        let leaves = sink.borrow();
        leaf_indices(template).into_iter().any(|index| {
            index
                .checked_sub(self.leaf_start)
                .and_then(|local| leaves.get(local))
                .is_some_and(|leaf| leaf.window || is_window_sql(&leaf.sql))
        })
    }

    /// Put the leaves of a template back as their SQL.
    ///
    /// A window's partition and ordering are part of the query that groups the rows, and that
    /// query reads the source: `AMONG [30d]` is written `(FLOOR(... t0."OrderDate" ...))` there,
    /// not `q_0.e_2` — a `SELECT` cannot refer to its own column aliases.
    fn expand_leaves(&self, template: &str) -> String {
        let Some(sink) = &self.leaves else {
            return template.to_string();
        };
        let leaves = sink.borrow();
        let mut sql = template.to_string();
        loop {
            let mut replaced = false;
            for index in leaf_indices(&sql) {
                let Some(leaf) = index
                    .checked_sub(self.leaf_start)
                    .and_then(|local| leaves.get(local))
                else {
                    continue;
                };
                let token = format!("{{leaf:{index}}}");
                if leaf.sql.contains(&token) {
                    continue;
                }
                sql = sql.replace(&token, &leaf.sql);
                replaced = true;
            }
            if !replaced {
                return sql;
            }
        }
    }

    /// The `{ext:N}` half of [`Codegen::hand_off`]: for callers without the plan layer.
    fn hand_off_external(&self, external: ExternalCall, call: Rendered) -> Rendered {
        let index = {
            let sink = self.externals.as_ref().expect("checked by the caller");
            let mut externals = sink.borrow_mut();
            externals.push(external);
            externals.len() - 1
        };
        Rendered {
            sql: format!("{{ext:{index}}}"),
            ..call
        }
    }

    fn field_type(&self, name: &str) -> DataType {
        self.types.get(name).copied().unwrap_or(DataType::String)
    }

    fn render_field(&self, name: &str) -> String {
        let quote = self.dialect.quote_char();
        match self.names.get(name) {
            Some(FieldName::Sql(sql)) => sql.clone(),
            Some(FieldName::Path(parts)) => parts
                .iter()
                .map(|part| quote_if_needed(part, quote))
                .collect::<Vec<String>>()
                .join("."),
            None => quote_if_needed(name, quote),
        }
    }

    fn render_unary(&self, op: UnOp, inner: &Item) -> Result<Rendered, String> {
        match op {
            UnOp::Plus => self.render(inner, 0),
            UnOp::Neg => {
                let arg = self.render(inner, PREC_NEG)?;
                self.apply_template("neg", &[arg], PREC_NEG)
            }
            UnOp::Not => {
                // SQLAlchemy pushes `NOT` into the operator: `NOT (a = 1)` is `a != 1`,
                // `NOT (a IN (1))` is `(a NOT IN (1))`, `NOT (a IS NULL)` is `a IS NOT NULL`.
                if let Some(negated) = self.render_negated(inner) {
                    return negated;
                }
                let arg = self.render(inner, PREC_NOT + 1)?;
                self.apply_template("not", &[arg], PREC_NOT)
            }
        }
    }

    fn render_binary(&self, op: BinOp, left: &Item, right: &Item) -> Result<Rendered, String> {
        let prec = op.precedence();
        let left = self.render(left, prec)?;
        let right = self.render(right, prec + 1)?;
        self.apply_template(op.name(), &[left, right], prec)
    }

    /// The SQL of `NOT item`, where `None` means "SQLAlchemy would wrap it in `NOT (...)`".
    ///
    /// `dl_formula` translates `NOT` with `sa.not_()`, and SQLAlchemy negates the operator
    /// instead of the result whenever it can (see `UnaryExpression._negate()`): a comparison
    /// flips to its opposite, `IS NULL` becomes `IS NOT NULL`, `IN` becomes `NOT IN`, and
    /// literals turn into comparisons with zero. Anything else keeps a plain `NOT (...)`.
    fn render_negated(&self, item: &Item) -> Option<Result<Rendered, String>> {
        let result = match item {
            Item::Binary(op, left, right) => {
                let opposite = match op {
                    BinOp::Eq => BinOp::Neq,
                    BinOp::Neq => BinOp::Eq,
                    BinOp::Lt => BinOp::Gte,
                    BinOp::Lte => BinOp::Gt,
                    BinOp::Gt => BinOp::Lte,
                    BinOp::Gte => BinOp::Lt,
                    BinOp::Like => BinOp::NotLike,
                    BinOp::NotLike => BinOp::Like,
                    _ => return None,
                };
                self.render_binary(opposite, left, right)
            }
            Item::Between {
                value,
                low,
                high,
                negated,
            } => self.render_between(value, low, high, !negated),
            Item::Is {
                value,
                kind,
                negated,
            } => {
                // PostgreSQL flips `IS NOT NULL` into `IS NULL`; MySQL and ClickHouse build
                // `IS NOT NULL` as `NOT ISNULL(x)`, and SQLAlchemy keeps a `NOT` around that
                // unary expression (`NOT (NOT ISNULL(x))`) instead of collapsing it.
                if *kind == IsKind::Null && *negated && self.dialect != Dialect::Postgres {
                    return None;
                }
                self.render_is(value, *kind, !negated)
            }
            Item::In {
                value,
                list,
                negated,
            } => self.render_negated_in(value, list, *negated),
            // `NOT 1` is `1 = 0` in every dialect; only MySQL does the same for booleans,
            // PostgreSQL and ClickHouse keep `NOT true` / `NOT 1`.
            Item::Num { .. } => self
                .render(item, 0)
                .map(|arg| comparison(format!("{} = 0", arg.sql))),
            Item::Bool(_) if self.dialect == Dialect::Mysql => self
                .render(item, 0)
                .map(|arg| comparison(format!("{} = 0", arg.sql))),
            _ => return None,
        };
        Some(result)
    }

    /// `value [NOT] BETWEEN low AND high`.
    fn render_between(
        &self,
        value: &Item,
        low: &Item,
        high: &Item,
        negated: bool,
    ) -> Result<Rendered, String> {
        let name = if negated { "notbetween" } else { "between" };
        let args = [
            self.render(value, 4)?,
            self.render(low, 4)?,
            self.render(high, 4)?,
        ];
        self.apply_template(name, &args, 3)
    }

    /// `value IS [NOT] (NULL|TRUE|FALSE)`.
    fn render_is(&self, value: &Item, kind: IsKind, negated: bool) -> Result<Rendered, String> {
        if kind == IsKind::Null {
            return if negated {
                self.render_is_not_null(value)
            } else {
                self.render_is_null(value)
            };
        }
        let arg = self.render(value, 4)?;
        let sql = match arg.dtype.base() {
            DataType::Integer | DataType::Float => {
                let op = if (kind == IsKind::True) != negated {
                    "!="
                } else {
                    "="
                };
                comparison(format!("{} {op} 0.0", arg.sql))
            }
            DataType::String | DataType::Geopoint | DataType::Geopolygon => {
                let op = if (kind == IsKind::True) != negated {
                    "!="
                } else {
                    "="
                };
                comparison(format!("{} {op} ''", arg.sql))
            }
            DataType::Boolean => match self.dialect {
                // ClickHouse has no boolean type: `istrue` compares with zero.
                Dialect::ClickHouse => {
                    let op = if (kind == IsKind::True) != negated {
                        "!="
                    } else {
                        "="
                    };
                    comparison(format!("{} {op} 0", arg.sql))
                }
                _ => {
                    let truth = if kind == IsKind::True {
                        "true"
                    } else {
                        "false"
                    };
                    let not = if negated { "NOT " } else { "" };
                    comparison(format!("{} IS {not}{truth}", arg.sql))
                }
            },
            // `istrue`/`isfalse` for dates are constants: `true`/`false` (and `1`/`0` in
            // ClickHouse). MySQL renders the negation of a boolean literal as `<lit> = 0`,
            // the others negate the literal itself (a literal needs no parentheses).
            DataType::Date | DataType::Datetime | DataType::GenericDatetime => {
                let truth = match (self.dialect, kind == IsKind::True) {
                    (Dialect::ClickHouse, true) => "1",
                    (Dialect::ClickHouse, false) => "0",
                    (_, true) => "true",
                    (_, false) => "false",
                };
                if !negated {
                    atom(truth)
                } else if self.dialect == Dialect::Mysql {
                    comparison(format!("{truth} = 0"))
                } else {
                    comparison(format!("NOT {truth}"))
                }
            }
            other => {
                return Err(format!(
                    "IS {} is not defined for {} in {}",
                    if kind == IsKind::True {
                        "TRUE"
                    } else {
                        "FALSE"
                    },
                    other.name(),
                    self.dialect.as_str()
                ))
            }
        };
        Ok(sql)
    }

    /// `value IS NULL`, with the operand parenthesized like SQLAlchemy does it.
    fn render_is_null(&self, value: &Item) -> Result<Rendered, String> {
        let arg = self.render_is_operand(value)?;
        Ok(self.isnull(arg))
    }

    /// `value IS NOT NULL`: PostgreSQL keeps the `IS NOT NULL` form, MySQL and ClickHouse
    /// have an `ISNULL(x)`/`isNull(x)` function and negate it.
    fn render_is_not_null(&self, value: &Item) -> Result<Rendered, String> {
        if self.dialect == Dialect::Postgres {
            let arg = self.render_is_operand(value)?;
            return Ok(comparison(format!("{} IS NOT NULL", arg.sql)));
        }
        let arg = self.render(value, 0)?;
        let isnull = self.isnull(arg);
        self.apply_template("not", &[isnull], PREC_NOT)
    }

    /// The `IS NULL` operand: PostgreSQL's `x IS NULL` needs a comparison-level operand
    /// (`(a = 1) IS NULL`), the MySQL/ClickHouse functions take the bare expression.
    fn render_is_operand(&self, value: &Item) -> Result<Rendered, String> {
        if self.dialect == Dialect::Postgres {
            self.render(value, 4)
        } else {
            self.render(value, 0)
        }
    }

    /// The `IS NULL`/`IS NOT NULL` forms used inside the `IN` NULL fix-up.
    ///
    /// `dl_formula` builds them with SQLAlchemy's `is_(None)`/`is_not(None)`, which print
    /// `x IS [NOT] NULL` in every dialect (no `ISNULL()`/`isNull()` there), with the operand
    /// parenthesized like a comparison.
    fn render_raw_is_null(&self, value: &Item) -> Result<Rendered, String> {
        let arg = self.render(value, 4)?;
        Ok(comparison(format!("{} IS NULL", arg.sql)))
    }

    fn render_raw_is_not_null(&self, value: &Item) -> Result<Rendered, String> {
        let arg = self.render(value, 4)?;
        Ok(comparison(format!("{} IS NOT NULL", arg.sql)))
    }

    /// `x IS NULL`, which `dl_formula` translates with `FuncIsnull`'s `V(D.DUMMY, ...)`:
    /// PostgreSQL keeps the `IS NULL` operator, MySQL and ClickHouse have a function of
    /// their own. It accepts any type (`D.DUMMY`), so it is rendered here instead of going
    /// through the registry, whose templates are only generated for the realistic types.
    fn isnull(&self, arg: Rendered) -> Rendered {
        comparison(match self.dialect {
            Dialect::Postgres => format!("{} IS NULL", arg.sql),
            Dialect::Mysql => format!("ISNULL({})", arg.sql),
            Dialect::ClickHouse => format!("isNull({})", arg.sql),
        })
    }

    /// `value [NOT] IN (a, b, ...)`.
    ///
    /// `dl_formula` renders `IN` with SQLAlchemy's `in_()` plus a NULL fix-up
    /// (`_in_fix_null`/`_not_in_fix_null`): `NULL` cannot be compared with `=`, so a list
    /// that contains it gets an extra `IS NULL` branch instead.
    fn render_in(&self, value: &Item, list: &[Item], negated: bool) -> Result<Rendered, String> {
        let (items, has_null) = split_nulls(list);
        let left = self.render_checked_left(value, &items)?;
        let list_sql = self.render_list(&items)?;

        if !negated {
            return if items.is_empty() {
                if has_null {
                    self.render_raw_is_null(value)
                } else {
                    // SQLAlchemy renders an empty `IN ()` as `x IN (NULL) AND (1 != 1)`.
                    Ok(boolean_and(format!("{} IN (NULL) AND (1 != 1)", left.sql)))
                }
            } else if has_null {
                let isnull = self.render_raw_is_null(value)?;
                Ok(boolean_or(format!(
                    "{} OR {} IN ({list_sql})",
                    isnull.sql, left.sql
                )))
            } else {
                Ok(comparison(format!("{} IN ({list_sql})", left.sql)))
            };
        }

        // `NOT IN`: the NULL fix-up wraps the comparison in `OR`/`AND` with the
        // `IS [NOT] NULL` branch, and SQLAlchemy always groups the `NOT IN` itself.
        if items.is_empty() {
            if has_null {
                return self.render_raw_is_not_null(value);
            }
            let notin = format!("{} NOT IN (NULL) OR (1 = 1)", left.sql);
            let isnull = self.render_raw_is_null(value)?;
            return Ok(boolean_or(format!("({notin}) OR {}", isnull.sql)));
        }
        let notin = format!("({} NOT IN ({list_sql}))", left.sql);
        if has_null {
            let isnotnull = self.render_raw_is_not_null(value)?;
            Ok(boolean_and(format!("{notin} AND {}", isnotnull.sql)))
        } else {
            let isnull = self.render_raw_is_null(value)?;
            Ok(boolean_or(format!("{notin} OR {}", isnull.sql)))
        }
    }

    /// The left operand of `IN` and of the `IS NULL` branch inside its NULL fix-up.
    ///
    /// An empty list has no element types to match against, but `dl_formula` still requires
    /// the operand to be one of `COMPARABLE_TYPES` can start with, so `IN ()` is rejected for
    /// `DATETIMETZ`/`MARKUP` and accepted for everything else.
    fn render_checked_left(&self, value: &Item, items: &[&Item]) -> Result<Rendered, String> {
        let left = self.render(value, 4)?;
        if items.is_empty() {
            return if COMPARABLE_TYPES
                .iter()
                .any(|(left_type, _)| left.dtype.casts_to(*left_type))
            {
                Ok(left)
            } else {
                Err(format!(
                    "IN is not defined for {} in {}",
                    left.dtype.base().name(),
                    self.dialect.as_str()
                ))
            };
        }
        for item in items {
            self.check_comparable(left.dtype, item)?;
        }
        Ok(left)
    }

    fn render_list(&self, items: &[&Item]) -> Result<String, String> {
        let mut rendered = Vec::with_capacity(items.len());
        for item in items {
            rendered.push(self.render(item, 0)?.sql);
        }
        Ok(rendered.join(", "))
    }

    /// `NOT (value [NOT] IN (...))`.
    ///
    /// `NOT IN` already expands to a boolean term (`x NOT IN (...) OR x IS NULL`), so its
    /// negation is a plain `NOT (...)`. Only `NOT IN (NULL)`, which collapses to
    /// `x IS NOT NULL`, negates back into `x IS NULL`.
    fn render_negated_in(
        &self,
        value: &Item,
        list: &[Item],
        negated: bool,
    ) -> Result<Rendered, String> {
        let (items, has_null) = split_nulls(list);
        if negated {
            return if items.is_empty() && has_null {
                self.render_raw_is_null(value)
            } else {
                let inner = self.render_in(value, list, true)?;
                Ok(not_wrapped(&inner.sql))
            };
        }
        let left = self.render_checked_left(value, &items)?;
        if items.is_empty() {
            // SQLAlchemy flips `NOT (x IN ())` into `(x NOT IN (NULL) OR (1 = 1))`.
            return Ok(atom(format!("({} NOT IN (NULL) OR (1 = 1))", left.sql)));
        }
        if has_null {
            let inner = self.render_in(value, list, false)?;
            return Ok(not_wrapped(&inner.sql));
        }
        let list_sql = self.render_list(&items)?;
        Ok(atom(format!("({} NOT IN ({list_sql}))", left.sql)))
    }

    /// `IN` compares the left operand with every list element; `dl_formula` raises a
    /// translation error for types that are not comparable, so the native engine reports a
    /// missing template and the caller falls back to the Python implementation.
    fn check_comparable(&self, left: DataType, item: &Item) -> Result<(), String> {
        let right = self.render(item, 0)?.dtype;
        let comparable = COMPARABLE_TYPES.iter().any(|(left_type, right_type)| {
            left.casts_to(*left_type) && right.casts_to(*right_type)
        });
        if comparable {
            Ok(())
        } else {
            Err(format!(
                "IN is not defined for {} and {} in {}",
                left.base().name(),
                right.base().name(),
                self.dialect.as_str()
            ))
        }
    }
}

impl<'a> Codegen<'a> {
    fn apply_template(
        &self,
        name: &str,
        args: &[Rendered],
        formula_prec: u8,
    ) -> Result<Rendered, String> {
        let signature = signature(args);
        let entry = registry::lookup(self.dialect.as_str(), name, args.len() as u8, &signature)
            // A window function called without a `WITHIN` clause is still a window function:
            // `RANK([m])` translates to `rank() OVER (ORDER BY ...)`, so the bare form is
            // stored under the `N|` prefix instead of failing the lookup.
            .or_else(|| {
                registry::lookup(
                    self.dialect.as_str(),
                    name,
                    args.len() as u8,
                    &format!("N|{}", signature),
                )
            })
            .ok_or_else(|| {
                format!(
                    "no translation for {}({}) in {}",
                    name.to_uppercase(),
                    signature,
                    self.dialect.as_str()
                )
            })?;
        let sqls: Vec<String> = args.iter().map(|arg| arg.sql.clone()).collect();
        Ok(Rendered {
            sql: substitute_slots(entry, &sqls),
            dtype: entry.ret,
            prec: template_prec(entry.template, formula_prec),
            token: entry.ret.name().to_string(),
        })
    }

    fn render_case(
        &self,
        operand: Option<&Item>,
        branches: &[(Item, Item)],
        else_: Option<&Item>,
    ) -> Result<Rendered, String> {
        let operand_sql = match operand {
            Some(item) => Some(self.render(item, 0)?.sql),
            None => None,
        };
        let mut result_types = Vec::new();
        let sql;

        // PostgreSQL always uses CASE. MySQL and ClickHouse render a single-branch IF as
        // IF(cond, then, else); multi-branch IF and CASE with an operand stay CASE.
        let use_if =
            operand_sql.is_none() && self.dialect != Dialect::Postgres && branches.len() == 1;
        if !use_if {
            let mut text = String::from("CASE");
            if let Some(operand_sql) = &operand_sql {
                text.push(' ');
                text.push_str(operand_sql);
            }
            for (cond, result) in branches {
                let cond = self.render(cond, 4)?;
                let result = self.render(result, 0)?;
                result_types.push(result.dtype);
                text.push_str(" WHEN ");
                text.push_str(&cond.sql);
                text.push_str(" THEN ");
                text.push_str(&result.sql);
            }
            if let Some(else_item) = else_ {
                let else_sql = self.render(else_item, 0)?;
                result_types.push(else_sql.dtype);
                text.push_str(" ELSE ");
                text.push_str(&else_sql.sql);
            }
            text.push_str(" END");
            sql = text;
        } else {
            let mut folded = match else_ {
                Some(item) => self.render(item, 0)?,
                None => Rendered {
                    sql: "NULL".to_string(),
                    dtype: DataType::Null,
                    prec: PREC_ATOM,
                    token: "NULL".to_string(),
                },
            };
            result_types.push(folded.dtype);
            let func = if self.dialect == Dialect::Mysql {
                "IF"
            } else {
                "if"
            };
            for (cond, result) in branches.iter().rev() {
                let cond_sql = self.render(cond, 0)?;
                let result_sql = self.render(result, 0)?;
                result_types.push(result_sql.dtype);
                folded = Rendered {
                    sql: format!(
                        "{func}({}, {}, {})",
                        cond_sql.sql, result_sql.sql, folded.sql
                    ),
                    dtype: result_sql.dtype,
                    prec: PREC_ATOM,
                    token: result_sql.dtype.name().to_string(),
                };
            }
            sql = folded.sql;
        }

        let dtype = unify(&result_types);
        Ok(Rendered {
            sql,
            dtype,
            prec: PREC_ATOM,
            token: dtype.name().to_string(),
        })
    }

    /// Translate a call, depending on the clauses written after its argument list.
    ///
    /// A window call is rendered here: `WITHIN`/`AMONG` become `PARTITION BY {W}`, and the
    /// call's own `ORDER BY` plus the request's default ordering become `ORDER BY {O}`. Three
    /// clauses do not change the SQL of the function itself — `TOTAL`, a LOD specifier and
    /// `BEFORE FILTER BY` move the field into a sub-query, so the call is rendered bare and
    /// handed to the plan ([`ExternalCall`]). Anything else is reported unsupported, because
    /// emitting the call without that sub-query would silently drop the filter or the level of
    /// detail.
    fn render_clause_call(
        &self,
        name: &str,
        args: &[Item],
        clauses: &Clauses,
    ) -> Result<Rendered, String> {
        if clauses.is_empty() {
            // These three are not registry templates in `dl_formula` either: the logical
            // functions are Python lambdas, `DB_CAST` reads a per-connector whitelist, and a
            // lookup is not a function at all — `dl_formula` replaces the call with its measure
            // and forks the query around it.
            if let Some(rendered) = self.logical_call(name, args)? {
                return Ok(rendered);
            }
            if let Some(rendered) = self.db_cast_call(name, args)? {
                return Ok(rendered);
            }
            if let Some(rendered) = self.lookup_call(name, args, clauses)? {
                return Ok(rendered);
            }
            return self.render_call(name, args);
        }
        let ordering = self.window_ordering(clauses)?;
        let partition = self.window_partition(clauses)?;
        if let Some(partition) = &partition {
            if clauses.lod.is_none()
                && clauses.bfb.is_empty()
                && clauses.ignore_dimensions.is_empty()
            {
                return self.render_window(name, args, partition, &ordering);
            }
        }
        if self.types_only {
            // The clauses of a call never change its type: `SUM(x) TOTAL` is still the type
            // `SUM(x)` returns, and `dl_formula` knows it before any fork is built. Only the
            // *translation* is unsupported, so a type query answers from the registry entry of
            // the bare call (a window function's entry has `{W}` in its template, but the same
            // return type).
            return self.render_call(name, args);
        }
        // `ExtAggregationToQueryForkMutation`: an aggregation with a LOD specifier is not an
        // expression the base query can select, it is a sub-query grouped by the resolved
        // dimensions and joined back. The bare call is rendered here, the caller turns it into
        // that sub-query, and the formula keeps a placeholder for the joined column.
        if clauses.lod.is_some()
            && clauses.grouping == Grouping::Default
            && clauses.bfb.is_empty()
            && clauses.ignore_dimensions.is_empty()
            && (self.externals.is_some() || self.leaves.is_some())
        {
            let call = self.render_call(name, args)?;
            let lod = clauses.lod.as_ref().expect("checked above");
            let mut dimensions = Vec::with_capacity(lod.dims.len());
            for dim in &lod.dims {
                dimensions.push(self.render(dim, 0)?.sql);
            }
            let external = ExternalCall {
                sql: call.sql.clone(),
                dimensions,
                kind: ExternalKind::Lod(lod.kind),
                data_type: call.dtype,
                skip_filters: Vec::new(),
                lookup: None,
            };
            return Ok(self.hand_off(external, call));
        }
        // `TOTAL` and `BEFORE FILTER BY` are the same fork as a LOD — only the grouping and the
        // filters differ. `SUM([a] TOTAL)` is one row for the whole source, so it groups by
        // nothing; a window with `BEFORE FILTER BY` groups by its `WITHIN` partition, but the
        // request filters on the named dimensions have to stay outside the fork, because the
        // window sees the rows before they were filtered.
        let fork_kind = match clauses.grouping {
            Grouping::Total => Some(ExternalKind::Total),
            Grouping::Within | Grouping::Among if !clauses.bfb.is_empty() => {
                Some(ExternalKind::Window)
            }
            _ => None,
        };
        if let Some(kind) = fork_kind {
            if clauses.lod.is_none()
                && clauses.ignore_dimensions.is_empty()
                && (self.externals.is_some() || self.leaves.is_some())
            {
                let call = self.render_call(name, args)?;
                let skip_filters = clauses
                    .bfb
                    .iter()
                    .map(|field| self.render_field(field))
                    .collect();
                let external = ExternalCall {
                    // The fork is its own query grouped by these dimensions, so the window
                    // inside it partitions by the rows the fork produced and has no `{W}`.
                    sql: self.fill_window(call.sql.clone(), &[], &ordering),
                    dimensions: partition.unwrap_or_default(),
                    kind,
                    data_type: call.dtype,
                    skip_filters,
                    lookup: None,
                };
                return Ok(self.hand_off(external, call));
            }
        }
        Err(format!(
            "{} with a {} clause is not translated natively",
            name.to_uppercase(),
            clause_names(clauses).join("/")
        ))
    }

    /// `AGO([measure], [date] [, unit [, number]])`: the previous value of `measure`.
    ///
    /// `AT_DATE([measure], [date], date_expression)`: the value of `measure` at a fixed date.
    ///
    /// `dl_formula` has no translation for either (`TimeSeriesFunction` raises "Lookup function
    /// should have been mutated"): `LookupFunctionToQueryForkMutation` turns them into the
    /// measure plus a `LEFT JOIN` of the same query, offset by `DATEADD` for `AGO` and compared
    /// with the date expression for `AT_DATE`. [`ExternalKind::Lookup`] is that hand-off. The
    /// result of the call is the measure, so the placeholder the formula gets is the measure's
    /// own expression.
    fn lookup_call(
        &self,
        name: &str,
        args: &[Item],
        clauses: &Clauses,
    ) -> Result<Option<Rendered>, String> {
        // `LOOKUP_MUTATOR_REGISTRY`: `dl_formula` 0.2457.0 has exactly two lookup mutators,
        // `AGO` and `AT_DATE`. A `LOOKUP` written by hand has no translation and no mutator, so
        // it must not fall through to the registry error: say what is missing instead.
        if name.eq_ignore_ascii_case("lookup") {
            return Err(
                "LOOKUP is not a function of dl_formula 0.2457.0 (its lookup mutators are AGO and \
                 AT_DATE), so it has no query fork to translate into"
                    .to_string(),
            );
        }
        let is_ago = name.eq_ignore_ascii_case("ago") && (2..=4).contains(&args.len());
        let is_at_date = name.eq_ignore_ascii_case("at_date") && args.len() == 3;
        if !is_ago && !is_at_date {
            return Ok(None);
        }
        if !clauses.bfb.is_empty() || !clauses.ignore_dimensions.is_empty() || clauses.lod.is_some()
        {
            return Ok(None);
        }
        let result = self.render(&args[0], 0)?;
        if self.types_only {
            // A type query only needs the return type, and both mutators have
            // `return_type = FromArgs(0)`. `dl_formula` raises before rendering the function,
            // so there is no translation to borrow it from — the argument is the answer.
            return Ok(Some(result));
        }
        if self.externals.is_none() && self.leaves.is_none() {
            // Without a plan there is nothing to fork. `dl_formula` says the same thing at the
            // same point ("Lookup function should have been mutated") instead of reporting a
            // missing translation, so the diagnostic names the mutation that is missing.
            return Err(format!(
                "the lookup function {} should have been mutated into a query fork",
                name.to_uppercase()
            ));
        }
        let dimension = self.render(&args[1], 0)?;
        // The offset is the dialect's `DATEADD` around the marker `{dim}`: the plan puts the
        // fork's own column there when it builds the join. `AT_DATE` joins on the date argument
        // itself and has no offset, so its fork side is the bare column.
        let marker = |sql: &str| Rendered {
            sql: sql.to_string(),
            dtype: dimension.dtype,
            prec: PREC_ATOM,
            token: dimension.token.clone(),
        };
        let (base, offset, day) = if is_at_date {
            let date = self.render(&args[2], 0)?;
            (date.sql, "{dim}".to_string(), None)
        } else {
            let mut dateadd = vec![marker("{dim}")];
            for arg in &args[2..] {
                dateadd.push(self.render(arg, 0)?);
            }
            let offset = self.apply_template("dateadd", &dateadd, PREC_ATOM)?.sql;
            // `_MONTH_BASED_UNITS`: `AGO` by a month must not match two different months that
            // share a day number twice (Jan 31 -> Feb 28 and Mar 31 -> Feb 28).
            let month_based = args.get(2).is_some_and(|unit| match unit {
                Item::Str(text) => matches!(
                    text.to_ascii_lowercase().as_str(),
                    "month" | "year" | "quarter"
                ),
                _ => false,
            });
            let day = month_based
                .then(|| {
                    self.apply_template("day", &[marker("{dim}")], PREC_ATOM)
                        .map(|rendered| rendered.sql)
                })
                .transpose()?;
            ("{dim}".to_string(), offset, day)
        };
        let mut skip_filters = Vec::new();
        for field in &clauses.bfb {
            skip_filters.push(self.render_field(field));
        }
        let external = ExternalCall {
            sql: result.sql.clone(),
            dimensions: vec![dimension.sql.clone()],
            kind: ExternalKind::Lookup,
            data_type: result.dtype,
            skip_filters,
            lookup: Some(Lookup {
                dimension: dimension.sql,
                base,
                offset,
                day,
            }),
        };
        // `QueryForkQuerySplitter.get_split_masks`: the default `BEFORE FILTER BY` of a lookup is
        // the lookup dimension itself, and a filter the mutation does not match stays in the
        // fork — the captured `AGO` SQL carries the same `WHERE` in both queries. A filter built
        // *on* the lookup dimension would have to be rewritten with `DATEADD` there; that is
        // still open, so the filter is kept as it is.
        Ok(Some(self.hand_off_forced(external, result)))
    }

    /// `ISTRUE(value)` / `ISFALSE(value)`: whether the value is truthy.
    ///
    /// `dl_formula` translates them as `value.is_(True)` / `value.is_(False)`
    /// (`operators_unary.UnaryIsTrue*`/`UnaryIsFalse*`), which is exactly the `IS TRUE`/`IS
    /// FALSE` spelling [`Codegen::render_is`] already emits for the operator: `x IS true` for a
    /// boolean, `x != 0.0` for a number, `x != ''` for a string or a geo type, a constant for a
    /// date. The registry has no rows for them (the translations are Python lambdas, not SQL
    /// templates), so they are rendered here instead of looked up.
    fn logical_call(&self, name: &str, args: &[Item]) -> Result<Option<Rendered>, String> {
        // `arg_cnt = 1` in `dl_formula`, so any other count is a formula error, not a call to
        // translate.
        if args.len() != 1 {
            return Ok(None);
        }
        let kind = if name.eq_ignore_ascii_case("istrue") {
            IsKind::True
        } else if name.eq_ignore_ascii_case("isfalse") {
            IsKind::False
        } else {
            return Ok(None);
        };
        Ok(Some(self.render_is(&args[0], kind, false)?))
    }

    /// `DB_CAST(expression, "native_type" [, param ...])`: an explicit cast to a database type.
    ///
    /// `dl_formula` does not template this one either: `FuncDbCastBase` builds a SQLAlchemy
    /// `cast()` from a per-connector whitelist of `(name, sa_type, params)`, validates the
    /// expression type and the parameter types, and lets SQLAlchemy print the type. The native
    /// table below is that whitelist, with the SQL type spelled the way SQLAlchemy prints it.
    fn db_cast_call(&self, name: &str, args: &[Item]) -> Result<Option<Rendered>, String> {
        if !name.eq_ignore_ascii_case("db_cast") {
            return Ok(None);
        }
        if !(2..=4).contains(&args.len()) {
            return Err(format!(
                "DB_CAST takes 2 to 4 arguments, got {}",
                args.len()
            ));
        }
        let value = self.render(&args[0], 0)?;
        if self.types_only {
            // `FuncDbCastBase.return_type = FromArgs(0)`: the formula's type is the argument's,
            // and `dl_formula` knows it before it validates the native type name.
            return Ok(Some(value));
        }
        let source = value.dtype.base();
        let Some(specs) = db_cast_types(self.dialect, source) else {
            return Err(format!(
                "Data type {} is not supported by function DB_CAST",
                source.name()
            ));
        };
        let Item::Str(native) = &args[1] else {
            return Err("the native type of DB_CAST must be a string literal".to_string());
        };
        let Some(spec) = specs
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(native))
        else {
            return Err(format!(
                "Native data type {} is not supported by function DB_CAST",
                native.to_ascii_uppercase()
            ));
        };
        if args.len() - 2 != spec.args as usize {
            return Err(format!(
                "Native type cast to {} requires {} parameters, got {}",
                spec.name.to_ascii_uppercase(),
                spec.args,
                args.len() - 2
            ));
        }
        let mut params = Vec::with_capacity(spec.args as usize);
        for arg in &args[2..] {
            let rendered = self.render(arg, 0)?;
            if !rendered.dtype.casts_to(DataType::ConstInteger) {
                return Err(format!(
                    "a parameter of the native type cast to {} has invalid type {}",
                    spec.name.to_ascii_uppercase(),
                    rendered.dtype.name()
                ));
            }
            params.push(rendered.sql);
        }
        Ok(Some(Rendered {
            sql: format!("CAST({} AS {})", value.sql, substitute(spec.sql, &params)),
            // `return_type = FromArgs(0)`.
            dtype: value.dtype,
            prec: PREC_ATOM,
            token: value.token,
        }))
    }

    /// The `PARTITION BY` list of a window call.
    ///
    /// `WITHIN` is used as written; `AMONG [d]` is `WITHIN <every dimension of the request
    /// except d>` — `dl_formula` rewrites it that way (`AmongToWithinGroupingMutation`) before
    /// anything is rendered, so an `AMONG` dimension the request does not group by is an error.
    ///
    /// `None` means the call has no partition of its own: `TOTAL` groups by everything, and a
    /// call without a grouping clause is not a window at all.
    fn window_partition(&self, clauses: &Clauses) -> Result<Option<Vec<String>>, String> {
        let mut dims = Vec::with_capacity(clauses.grouping_dims.len());
        for dim in &clauses.grouping_dims {
            dims.push(self.render(dim, 0)?.sql);
        }
        match clauses.grouping {
            Grouping::Within => Ok(Some(dims)),
            Grouping::Among => {
                // A type query has no request around it (`infer_type` renders with the fields
                // unsubstituted and an empty context), and the partition does not change the
                // type of the call: `MAX([x] AMONG [d])` is still the type of `MAX`.
                if self.types_only {
                    return Ok(Some(dims));
                }
                for dim in &dims {
                    // The request's dimensions are lifted into leaves of their own, so a dimension
                    // has to be compared as the SQL it stands for — `AMONG [30d]` names the same
                    // dimension either way.
                    if !self.window.group_dims.iter().any(|global| {
                        global == dim || self.expand_leaves(global) == self.expand_leaves(dim)
                    }) {
                        return Err(format!(
                            "{dim} is not a dimension of the request, so AMONG cannot use it"
                        ));
                    }
                }
                Ok(Some(
                    self.window
                        .group_dims
                        .iter()
                        .filter(|global| !dims.contains(global))
                        .cloned()
                        .collect(),
                ))
            }
            Grouping::Total | Grouping::Default => Ok(None),
        }
    }

    /// The ordering of a window call: `DefaultWindowOrderingMutation`.
    ///
    /// A function with `uses_default_ordering` (`RSUM`, `MAVG`, ...) takes the request's
    /// `ORDER BY` when it has none of its own — without it `RSUM` sums the whole partition —
    /// and appends whatever the request orders by that the call does not already mention.
    fn window_ordering(&self, clauses: &Clauses) -> Result<Vec<String>, String> {
        let mut items: Vec<(String, bool)> = Vec::with_capacity(clauses.ordering.len());
        for (item, direction) in &clauses.ordering {
            items.push((
                self.render(item, 0)?.sql,
                *direction == OrderingDirection::Desc,
            ));
        }
        items.extend(self.request_ordering());
        Ok(window_order_items(items))
    }

    /// The `ORDER BY` of the request, as [`WindowContext::ordering`] holds it.
    fn request_ordering(&self) -> Vec<(String, bool)> {
        let mut items: Vec<(String, bool)> = Vec::with_capacity(self.window.ordering.len());
        for (sql, desc) in &self.window.ordering {
            if items.iter().any(|(existing, _)| existing == sql) {
                continue;
            }
            items.push((sql.clone(), *desc));
        }
        items
    }

    /// Fill the slots of a window template: `{W}` with the partition, `{O}` with the ordering.
    ///
    /// Without an ordering the `{O}` slot and the space that separated it are dropped, which is
    /// the SQL `dl_formula` renders for the same call without a default ordering:
    /// `AVG(x) OVER (ORDER BY {O} ROWS ...)` becomes `AVG(x) OVER (ROWS ...)`.
    fn fill_window(&self, sql: String, partition: &[String], ordering: &[String]) -> String {
        let sql = sql.replace("{W}", &partition.join(", "));
        if !sql.contains("{O}") {
            return sql;
        }
        if ordering.is_empty() {
            // The space that separated the slot from the frame is dropped with the slot.
            if sql.contains("ORDER BY {O} ") {
                sql.replace("ORDER BY {O} ", "")
            } else {
                sql.replace(" ORDER BY {O}", "")
            }
        } else {
            sql.replace("{O}", &ordering.join(", "))
        }
    }

    fn render_call(&self, name: &str, args: &[Item]) -> Result<Rendered, String> {
        // `FuncDatetimeTZToNaivePG`/`_CH` read the timezone from the argument's
        // `data_type_params` (`value_ctx.data_type_params.timezone`) and render
        // `timezone(tz, CAST(x AS TIMESTAMPTZ))`. `dfl_native::DataType` carries no type
        // parameters, so the timezone is not available and a UTC guess would silently change
        // the value; say what is missing instead of the generic registry error.
        if name.eq_ignore_ascii_case("datetimetz_to_naive") {
            return Err(
                "DATETIMETZ_TO_NAIVE needs the timezone carried by its DATETIMETZ argument's \
                 data_type_params, which dfl_native's type model does not have"
                    .to_string(),
            );
        }
        // A window function called without a clause of its own (`RSUM([a])`) is still a window:
        // `is_window_call` with no grouping says whether an argument with a window in it has to
        // move down a level.
        let rendered = self.render_args(args, func_props::is_window_call(name, false))?;
        let call = self.apply_template(name, &rendered, PREC_ATOM)?;
        // A window function called without `WITHIN` is still a window function: the registry
        // keeps the bare form under the `N|` prefix, with a `{O}` slot and no partition. The
        // ordering is the request's (`DefaultWindowOrderingMutation`), so the slot is filled
        // here; a template without slots comes back unchanged.
        if !call.sql.contains('{') {
            return Ok(call);
        }
        Ok(Rendered {
            sql: self.fill_window(call.sql, &[], &window_order_items(self.request_ordering())),
            ..call
        })
    }

    fn render_window(
        &self,
        name: &str,
        args: &[Item],
        partition: &[String],
        ordering: &[String],
    ) -> Result<Rendered, String> {
        let rendered = self.render_args(args, true)?;
        let sqls: Vec<String> = rendered.iter().map(|arg| arg.sql.clone()).collect();
        // A window without a partition is the bare form (`RSUM([a])` with no `WITHIN`):
        // `OVER ()` and `OVER (PARTITION BY )` are different queries, and the registry keeps a
        // row for each.
        let prefixes: [&str; 2] = if partition.is_empty() {
            ["N", "W"]
        } else {
            ["W", "N"]
        };
        let mut found = None;
        for prefix in prefixes {
            let window_signature = format!("{prefix}|{}", signature(&rendered));
            if let Some(entry) = registry::lookup(
                self.dialect.as_str(),
                name,
                args.len() as u8,
                &window_signature,
            ) {
                found = Some((prefix, entry));
                break;
            }
        }
        let Some((prefix, entry)) = found else {
            return Err(format!(
                "no window translation for {}({}) in {}",
                name.to_uppercase(),
                signature(&rendered),
                self.dialect.as_str()
            ));
        };
        let sql = substitute_slots(entry, &sqls);
        let sql = self.fill_window(sql, if prefix == "W" { partition } else { &[] }, ordering);
        Ok(Rendered {
            sql,
            dtype: entry.ret,
            prec: PREC_ATOM,
            token: entry.ret.name().to_string(),
        })
    }
}

/// Whether rendered SQL calls a window function.
///
/// Every window template in [`crate::registry`] spells the clause as `... OVER (...)`, so a
/// substituted reference that carries a window (`[precalc]`, the `CASE` another field's formula
/// rendered) is recognised the same way a node of the tree is.
pub fn is_window_sql(sql: &str) -> bool {
    sql.contains(" OVER (")
}

/// Fork every window that another window is built from.
///
/// `WinFuncQuerySplitter` keeps a window function as the topmost node of its query, so a window
/// inside the argument of another one cannot be part of the same `SELECT` — Postgres rejects the
/// nesting (`window function calls cannot be nested`, SQLSTATE 42P20). The inner window becomes a
/// fork grouped by the request's dimensions (exactly the rows the outer window sees), and the
/// outer one reads the column it selects.
///
/// Only the leaves are visible here: a formula compiled on its own already lifted its windows
/// into leaves, and a reference to another field is the leaf that field's formula left. A window
/// written *inside* another window in one formula is split off as it is rendered
/// ([`Codegen::hand_off_window_argument`]) — this is the other half, for the leaves a formula
/// found already built.
///
/// `dimensions` is the `WindowContext::group_dims` of the request. Returns the number of leaves
/// that became forks.
pub fn fork_nested_windows(leaves: &mut [Leaf], dimensions: &[String]) -> usize {
    let mut forked = 0;
    for index in 0..leaves.len() {
        if !leaves[index].window || leaves[index].external.is_some() {
            continue;
        }
        let nested: Vec<usize> = leaf_indices(&leaves[index].sql)
            .into_iter()
            .filter(|other| *other != index && *other < leaves.len())
            .filter(|other| window_leaf(&leaves[*other]))
            .collect();
        for other in nested {
            if leaves[other].external.is_some() {
                // Already a query of its own: the outer window reads its column.
                continue;
            }
            let external = ExternalCall {
                sql: leaves[other].sql.clone(),
                dimensions: dimensions.to_vec(),
                kind: ExternalKind::Window,
                data_type: leaves[other].data_type,
                skip_filters: Vec::new(),
                lookup: None,
            };
            leaves[other].external = Some(external);
            forked += 1;
        }
    }
    forked
}

/// Whether a leaf computes a window, i.e. it is one or it is built from one.
fn window_leaf(leaf: &Leaf) -> bool {
    leaf.window || is_window_sql(&leaf.sql)
}

/// The indices of the `{leaf:N}` placeholders a template mentions.
fn leaf_indices(template: &str) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{leaf:") {
        let after = &rest[start + "{leaf:".len()..];
        let Some(end) = after.find('}') else {
            break;
        };
        if let Ok(index) = after[..end].parse::<usize>() {
            indices.push(index);
        }
        rest = &after[end..];
    }
    indices
}

/// The leaf a template is, when it is nothing but one placeholder: `{leaf:4}`.
fn single_leaf(template: &str) -> Option<usize> {
    let indices = leaf_indices(template);
    let index = *indices.first()?;
    (indices.len() == 1 && template == format!("{{leaf:{index}}}")).then_some(index)
}

/// Whether the tree contains a window call of its own.
///
/// `MAX(RMAX([x] AMONG [d]) AMONG [d2])` is a window over a window: the inner one has to be
/// computed by a query of its own, see [`Codegen::hand_off_window_argument`].
fn contains_window(item: &Item) -> bool {
    match item {
        Item::Field(_)
        | Item::Num { .. }
        | Item::Str(_)
        | Item::Date { .. }
        | Item::Bool(_)
        | Item::Null => false,
        Item::Unary(_, inner) => contains_window(inner),
        Item::Binary(_, left, right) => contains_window(left) || contains_window(right),
        Item::In { value, list, .. } => contains_window(value) || list.iter().any(contains_window),
        Item::Is { value, .. } => contains_window(value),
        Item::Case {
            operand,
            branches,
            else_,
        } => {
            operand.as_deref().is_some_and(contains_window)
                || branches
                    .iter()
                    .any(|(when, then)| contains_window(when) || contains_window(then))
                || else_.as_deref().is_some_and(contains_window)
        }
        Item::Between {
            value, low, high, ..
        } => contains_window(value) || contains_window(low) || contains_window(high),
        Item::Call {
            name,
            args,
            clauses,
        } => {
            func_props::is_window_call(name, clauses.grouping != Grouping::Default)
                || args.iter().any(contains_window)
        }
    }
}

/// Render window ordering items the way `ORDER BY` spells them.
fn window_order_items(items: Vec<(String, bool)>) -> Vec<String> {
    items
        .into_iter()
        .map(|(sql, desc)| if desc { format!("{sql} DESC") } else { sql })
        .collect()
}

/// Render a template, honouring the argument-order map of the entry.
fn substitute_slots(entry: &registry::Entry, sqls: &[String]) -> String {
    let mut ordered: Vec<&str> = Vec::with_capacity(sqls.len());
    for (slot, _) in sqls.iter().enumerate() {
        let idx = entry.arg_index(slot);
        ordered.push(sqls.get(idx).map(String::as_str).unwrap_or(""));
    }
    let owned: Vec<String> = ordered.iter().map(|sql| sql.to_string()).collect();
    // The registry stores the templates as `dl_formula` renders them, i.e. as SQLAlchemy output,
    // and SQLAlchemy escapes `%` for the DBAPI's `%`-paramstyle: the modulo operator is
    // `{0} %% {1}` and a `LIKE` pattern is `LIKE '%%week%%'`. The driver halves them again before
    // the server parses the statement, so the database reads one `%` — which is what this engine
    // has to write, because it builds the statement itself (see `Item::Str` in `render`). Without
    // this, `[a] % 2` would be a syntax error and `CONTAINS(...)` would search for a literal
    // `%week%`.
    substitute(&entry.template.replace("%%", "%"), &owned)
}

/// One native type `DB_CAST` accepts, from `FuncDbCastBase.WHITELISTS` of `dl_formula`.
struct DbCastType {
    /// The whitelist name, matched case-insensitively (`name.upper()` in the reference).
    name: &'static str,
    /// The SQL type, spelled the way SQLAlchemy's type compiler prints it. `{0}`/`{1}` are the
    /// parameters the cast takes (a precision, a length, ...).
    sql: &'static str,
    /// How many constant parameters the type takes.
    args: u8,
}

/// `FuncDbCastPostgreSQLBase.WHITELISTS` for the source types `DbCastArgTypes` allows.
fn db_cast_types(dialect: Dialect, source: DataType) -> Option<&'static [DbCastType]> {
    const PG_INT: &[DbCastType] = &[
        DbCastType {
            name: "smallint",
            sql: "SMALLINT",
            args: 0,
        },
        DbCastType {
            name: "integer",
            sql: "INTEGER",
            args: 0,
        },
        DbCastType {
            name: "bigint",
            sql: "BIGINT",
            args: 0,
        },
    ];
    const PG_FLOAT: &[DbCastType] = &[
        DbCastType {
            name: "double precision",
            sql: "DOUBLE PRECISION",
            args: 0,
        },
        DbCastType {
            name: "real",
            sql: "REAL",
            args: 0,
        },
        DbCastType {
            name: "numeric",
            sql: "NUMERIC({0}, {1})",
            args: 2,
        },
    ];
    const PG_STRING: &[DbCastType] = &[
        DbCastType {
            name: "text",
            sql: "TEXT",
            args: 0,
        },
        DbCastType {
            name: "character",
            sql: "CHAR({0})",
            args: 1,
        },
        DbCastType {
            name: "character varying",
            sql: "VARCHAR({0})",
            args: 1,
        },
        // The reference allows the aliases too, "just because they are more widely used".
        DbCastType {
            name: "char",
            sql: "CHAR({0})",
            args: 1,
        },
        DbCastType {
            name: "varchar",
            sql: "VARCHAR({0})",
            args: 1,
        },
    ];
    const PG_ARRAY_STR: &[DbCastType] = &[
        DbCastType {
            name: "text[]",
            sql: "TEXT[]",
            args: 0,
        },
        DbCastType {
            name: "character varying[]",
            sql: "VARCHAR[]",
            args: 0,
        },
        DbCastType {
            name: "varchar[]",
            sql: "VARCHAR[]",
            args: 0,
        },
    ];
    const PG_ARRAY_INT: &[DbCastType] = &[
        DbCastType {
            name: "smallint[]",
            sql: "SMALLINT[]",
            args: 0,
        },
        DbCastType {
            name: "integer[]",
            sql: "INTEGER[]",
            args: 0,
        },
        DbCastType {
            name: "bigint[]",
            sql: "BIGINT[]",
            args: 0,
        },
    ];
    const PG_ARRAY_FLOAT: &[DbCastType] = &[
        DbCastType {
            name: "double precision[]",
            sql: "DOUBLE PRECISION[]",
            args: 0,
        },
        DbCastType {
            name: "real[]",
            sql: "REAL[]",
            args: 0,
        },
        DbCastType {
            name: "numeric[]",
            sql: "NUMERIC({0}, {1})[]",
            args: 2,
        },
    ];
    // `clickhouse_sqlalchemy` prints these class names as-is, and `FuncDbCastClickHouseBase`
    // wraps the whole type in `Nullable(...)`.
    const CH_INT: &[DbCastType] = &[
        DbCastType {
            name: "Int8",
            sql: "Nullable(Int8)",
            args: 0,
        },
        DbCastType {
            name: "Int16",
            sql: "Nullable(Int16)",
            args: 0,
        },
        DbCastType {
            name: "Int32",
            sql: "Nullable(Int32)",
            args: 0,
        },
        DbCastType {
            name: "Int64",
            sql: "Nullable(Int64)",
            args: 0,
        },
        DbCastType {
            name: "UInt8",
            sql: "Nullable(UInt8)",
            args: 0,
        },
        DbCastType {
            name: "UInt16",
            sql: "Nullable(UInt16)",
            args: 0,
        },
        DbCastType {
            name: "UInt32",
            sql: "Nullable(UInt32)",
            args: 0,
        },
        DbCastType {
            name: "UInt64",
            sql: "Nullable(UInt64)",
            args: 0,
        },
    ];
    const CH_FLOAT: &[DbCastType] = &[
        DbCastType {
            name: "Float32",
            sql: "Nullable(Float32)",
            args: 0,
        },
        DbCastType {
            name: "Float64",
            sql: "Nullable(Float64)",
            args: 0,
        },
        DbCastType {
            name: "Decimal",
            sql: "Nullable(Decimal({0}, {1}))",
            args: 2,
        },
    ];
    const CH_STRING: &[DbCastType] = &[DbCastType {
        name: "String",
        sql: "Nullable(String)",
        args: 0,
    }];
    const CH_DATE: &[DbCastType] = &[
        DbCastType {
            name: "Date",
            sql: "Nullable(Date)",
            args: 0,
        },
        DbCastType {
            name: "Date32",
            sql: "Nullable(Date32)",
            args: 0,
        },
    ];
    match (dialect, source) {
        (Dialect::Postgres, DataType::Integer) => Some(PG_INT),
        (Dialect::Postgres, DataType::Float) => Some(PG_FLOAT),
        (Dialect::Postgres, DataType::String) => Some(PG_STRING),
        (Dialect::Postgres, DataType::ArrayStr) => Some(PG_ARRAY_STR),
        (Dialect::Postgres, DataType::ArrayInt) => Some(PG_ARRAY_INT),
        (Dialect::Postgres, DataType::ArrayFloat) => Some(PG_ARRAY_FLOAT),
        (Dialect::ClickHouse, DataType::Integer) => Some(CH_INT),
        (Dialect::ClickHouse, DataType::Float) => Some(CH_FLOAT),
        (Dialect::ClickHouse, DataType::String) => Some(CH_STRING),
        (Dialect::ClickHouse, DataType::Date) => Some(CH_DATE),
        // MySQL and every other source type `DbCastArgTypes` accepts are not whitelisted by any
        // connector of the reference, so `dl_formula` has no translation for them either.
        _ => None,
    }
}

fn signature(args: &[Rendered]) -> String {
    args.iter()
        .map(|arg| arg.token.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// The clauses a call carries, for the "unsupported" diagnostic.
fn clause_names(clauses: &Clauses) -> Vec<&'static str> {
    let mut names = Vec::new();
    match clauses.grouping {
        Grouping::Total => names.push("TOTAL"),
        Grouping::Among => names.push("AMONG"),
        // A `WITHIN` without dimensions is not a partition, it is an empty `OVER ()`.
        Grouping::Within => names.push("WITHIN"),
        Grouping::Default => {}
    }
    if !clauses.ordering.is_empty() {
        names.push("ORDER BY");
    }
    if clauses.lod.is_some() {
        names.push("LOD");
    }
    if !clauses.bfb.is_empty() {
        names.push("BEFORE FILTER BY");
    }
    if !clauses.ignore_dimensions.is_empty() {
        names.push("IGNORE DIMENSIONS");
    }
    names
}

/// The list of `IN` arguments without `NULL`s, plus a flag telling whether there were any.
///
/// `dl_formula` drops `NULL` literals before building the `IN` (SQLAlchemy cannot compare
/// with `=` against `NULL`) and adds an `IS NULL`/`IS NOT NULL` branch instead.
fn split_nulls(list: &[Item]) -> (Vec<&Item>, bool) {
    let mut items = Vec::with_capacity(list.len());
    let mut has_null = false;
    for item in list {
        if matches!(item, Item::Null) {
            has_null = true;
        } else {
            items.push(item);
        }
    }
    (items, has_null)
}

fn comparison(sql: String) -> Rendered {
    Rendered {
        sql,
        dtype: DataType::Boolean,
        prec: 3,
        token: "BOOLEAN".to_string(),
    }
}

fn boolean_and(sql: String) -> Rendered {
    Rendered {
        sql,
        dtype: DataType::Boolean,
        prec: 2,
        token: "BOOLEAN".to_string(),
    }
}

fn boolean_or(sql: String) -> Rendered {
    Rendered {
        sql,
        dtype: DataType::Boolean,
        prec: 1,
        token: "BOOLEAN".to_string(),
    }
}

fn atom(sql: impl Into<String>) -> Rendered {
    Rendered {
        sql: sql.into(),
        dtype: DataType::Boolean,
        prec: PREC_ATOM,
        token: "BOOLEAN".to_string(),
    }
}

fn not_wrapped(sql: impl Into<String>) -> Rendered {
    Rendered {
        sql: format!("NOT ({})", sql.into()),
        dtype: DataType::Boolean,
        prec: PREC_NOT,
        token: "BOOLEAN".to_string(),
    }
}

fn substitute(template: &str, sqls: &[String]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    for (idx, chunk) in template.split('{').enumerate() {
        if idx == 0 {
            out.push_str(chunk);
            continue;
        }
        match chunk.split_once('}') {
            Some((placeholder, rest)) => {
                match placeholder
                    .parse::<usize>()
                    .ok()
                    .and_then(|pos| sqls.get(pos))
                {
                    Some(sql) => out.push_str(sql),
                    None => {
                        out.push('{');
                        out.push_str(placeholder);
                        out.push('}');
                    }
                }
                out.push_str(rest);
            }
            None => {
                out.push('{');
                out.push_str(chunk);
            }
        }
    }
    out
}

/// Whether the SQL produced by a template needs parentheses in a bigger expression.
fn template_prec(template: &str, fallback: u8) -> u8 {
    let trimmed = template.trim_start();
    if trimmed.starts_with("CASE") {
        return PREC_ATOM;
    }
    let starts_with_name = trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_');
    if starts_with_name {
        if let Some(open) = trimmed.find('(') {
            let mut depth = 0i32;
            for (pos, ch) in trimmed.char_indices().skip(open) {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let rest = trimmed[pos + 1..].trim();
                            return if rest.is_empty() { PREC_ATOM } else { fallback };
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    fallback
}

/// The SQL text of a numeric literal, exactly as `dl_formula` renders it.
///
/// `dl_formula` parses the lexeme into a Python `int`/`float` and lets SQLAlchemy print it
/// with `literal_binds`, which is `str(int)` for integers (exact, arbitrary length) and
/// `repr(float)` for floats.
fn format_number(text: &str, is_int: bool) -> String {
    if is_int {
        format_int(text)
    } else {
        format_float(text)
    }
}

/// `str(int(<text>))`: exact for any length, leading zeros dropped.
fn format_int(text: &str) -> String {
    let digits = text.trim_start_matches('0');
    if digits.is_empty() {
        "0".to_string()
    } else {
        digits.to_string()
    }
}

/// Python's `repr(float)`.
fn format_float(text: &str) -> String {
    let value: f64 = text.parse().unwrap_or(0.0);
    // Rust's `Debug` for `f64` is also the shortest round-trip representation, so the digits
    // match `repr`; only the exponent is spelled differently (`1e-7` vs `1e-07`, `1e16` vs
    // `1e+16`), which `normalize_exponent` aligns.
    normalize_exponent(&format!("{value:?}"))
}

/// `1e16` -> `1e+16`, `1.5e-7` -> `1.5e-07`: Python always writes a sign and at least two
/// exponent digits.
fn normalize_exponent(text: &str) -> String {
    let Some(pos) = text.find(['e', 'E']) else {
        return text.to_string();
    };
    let (mantissa, exp) = text.split_at(pos);
    let exp = &exp[1..];
    let (sign, digits) = match exp.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
    };
    format!("{mantissa}e{sign}{digits:0>2}")
}

const RESERVED: &[&str] = &[
    "all",
    "and",
    "as",
    "asc",
    "between",
    "by",
    "case",
    "cast",
    "check",
    "column",
    "create",
    "current_date",
    "current_timestamp",
    "default",
    "delete",
    "desc",
    "distinct",
    "else",
    "end",
    "exists",
    "false",
    "from",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "interval",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "right",
    "select",
    "table",
    "then",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "when",
    "where",
];

fn quote_if_needed(part: &str, quote: char) -> String {
    let simple = !part.is_empty()
        && part
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && part
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.contains(&part.to_ascii_lowercase().as_str());
    if simple {
        part.to_string()
    } else {
        let escaped = part.replace(quote, &format!("{quote}{quote}"));
        format!("{quote}{escaped}{quote}")
    }
}

/// Type of a `CASE`/`IF` expression: an approximation of `dl_formula`'s type merging,
/// good enough to pick the outer translation.
fn unify(types: &[DataType]) -> DataType {
    let mut result: Option<DataType> = None;
    for dtype in types {
        let dtype = dtype.base();
        result = Some(match result {
            None => dtype,
            Some(current) if current == dtype => current,
            Some(current) => merge_pair(current, dtype),
        });
    }
    result.unwrap_or(DataType::Unsupported)
}

fn merge_pair(left: DataType, right: DataType) -> DataType {
    use DataType::*;
    let numeric = |dtype: DataType| matches!(dtype, Integer | Float);
    if numeric(left) && numeric(right) {
        return if left == Float || right == Float {
            Float
        } else {
            Integer
        };
    }
    let datetime =
        |dtype: DataType| matches!(dtype, Date | Datetime | Datetimetz | GenericDatetime);
    if datetime(left) && datetime(right) {
        return Datetime;
    }
    if left == String || right == String {
        return String;
    }
    if matches!(left, Unsupported | Null) {
        return right;
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_formula_splits_into_the_leaves_a_fork_needs() {
        // `COUNTD([a]) / COUNTD([a] FIXED)`: the two aggregates are what the plan selects
        // (`q_0.e_N`, `q_1.e_M`), the division stays in the outer query.
        let fields = FieldTypes::new();
        let names = FieldNames::new();
        let item = crate::parse_formula("COUNTD([a]) / COUNTD([a] FIXED)").unwrap();
        let compiled = compile_leaves(&item, Dialect::Postgres, &fields, &names).unwrap();
        assert_eq!(compiled.leaves.len(), 2);
        assert_eq!(compiled.leaves[0].sql, "count(DISTINCT a)");
        assert!(compiled.leaves[0].aggregated);
        assert!(compiled.leaves[0].external.is_none());
        assert_eq!(compiled.leaves[1].sql, "count(DISTINCT a)");
        let external = compiled.leaves[1].external.as_ref().unwrap();
        assert_eq!(external.kind, ExternalKind::Lod(LodKind::Fixed));
        assert!(external.dimensions.is_empty());
        assert!(compiled.sql.contains("{leaf:0}"), "{}", compiled.sql);
        assert!(compiled.sql.contains("{leaf:1}"), "{}", compiled.sql);
        assert!(!compiled.sql.contains("count"), "{}", compiled.sql);

        // `compile` stays strict: without a plan the LOD clause cannot be dropped silently.
        assert!(compile_named(&item, Dialect::Postgres, &fields, &names).is_err());
    }

    #[test]
    fn a_lod_call_becomes_an_external_and_leaves_a_placeholder() {
        // `ExtAggregationToQueryForkMutation`: `COUNTD([a] FIXED [b])` is not a column
        // expression — the query layer computes it in a sub-query grouped by `[b]` and joins
        // it back. The type and the SQL of the call are still the translator's job.
        let fields = FieldTypes::new();
        let names = FieldNames::new();
        let item = crate::parse_formula("COUNTD([a] FIXED [b])").unwrap();
        let (compiled, externals) =
            compile_external(&item, Dialect::Postgres, &fields, &names).unwrap();
        assert_eq!(compiled.sql, "{ext:0}");
        assert_eq!(compiled.data_type, DataType::Integer);
        assert_eq!(externals.len(), 1);
        assert_eq!(externals[0].sql, "count(DISTINCT a)");
        assert_eq!(externals[0].dimensions, vec!["b".to_string()]);
        assert_eq!(externals[0].kind, ExternalKind::Lod(LodKind::Fixed));
        assert_eq!(externals[0].data_type, DataType::Integer);
    }

    #[test]
    fn total_and_before_filter_by_fork_like_a_lod() {
        // `SUM([a] TOTAL)` is one row for the whole source: same fork as an ungrouped LOD.
        let mut fields = FieldTypes::new();
        fields.insert("a".to_string(), DataType::Integer);
        fields.insert("b".to_string(), DataType::String);
        fields.insert("c".to_string(), DataType::String);
        let names = FieldNames::new();
        let item = crate::parse_formula("SUM([a] TOTAL)").unwrap();
        let (compiled, externals) =
            compile_external(&item, Dialect::Postgres, &fields, &names).unwrap();
        assert_eq!(compiled.sql, "{ext:0}");
        assert_eq!(externals.len(), 1);
        assert_eq!(externals[0].sql, "sum(a)");
        assert_eq!(externals[0].kind, ExternalKind::Total);
        assert!(externals[0].dimensions.is_empty());
        assert!(externals[0].skip_filters.is_empty());

        // A window with `BEFORE FILTER BY` groups by its partition, and the request filters on
        // the named dimension stay outside the fork.
        let names: FieldNames = [("b".to_string(), FieldName::Sql("t0.\"b\"".to_string()))]
            .into_iter()
            .collect();
        let item = crate::parse_formula("SUM([a] WITHIN [c] BEFORE FILTER BY [b])").unwrap();
        let (compiled, externals) =
            compile_external(&item, Dialect::Postgres, &fields, &names).unwrap();
        assert_eq!(compiled.sql, "{ext:0}");
        let external = &externals[0];
        assert_eq!(external.kind, ExternalKind::Window);
        assert_eq!(external.sql, "sum(a)");
        assert_eq!(external.dimensions, vec!["c".to_string()]);
        assert_eq!(external.skip_filters, vec!["t0.\"b\"".to_string()]);

        // Without a plan the clause is still a gap: dropping `BEFORE FILTER BY` silently would
        // make the window see filtered rows.
        let item = crate::parse_formula("SUM([a] AMONG [c])").unwrap();
        assert!(compile_named(&item, Dialect::Postgres, &fields, &names).is_err());
    }

    #[test]
    fn integers_keep_every_digit() {
        // `dl_formula` keeps integers as Python `int` and prints them with `str(int)`.
        assert_eq!(
            format_number("9223372036854775807", true),
            "9223372036854775807"
        );
        assert_eq!(format_number("9007199254740993", true), "9007199254740993");
        assert_eq!(
            format_number("123456789012345678901234567890", true),
            "123456789012345678901234567890"
        );
        assert_eq!(format_number("007", true), "7");
        assert_eq!(format_number("0", true), "0");
    }

    #[test]
    fn floats_use_python_repr() {
        // Reference values are `repr()` of the same literal in Python 3.
        assert_eq!(format_number("1e10", false), "10000000000.0");
        assert_eq!(format_number("1e15", false), "1000000000000000.0");
        assert_eq!(format_number("1e16", false), "1e+16");
        assert_eq!(format_number("1e-7", false), "1e-07");
        assert_eq!(format_number("0.00001", false), "1e-05");
        assert_eq!(format_number("0.0001", false), "0.0001");
        assert_eq!(format_number("1.5e-10", false), "1.5e-10");
        assert_eq!(
            format_number("0.30000000000000004", false),
            "0.30000000000000004"
        );
        assert_eq!(format_number("1e300", false), "1e+300");
        assert_eq!(format_number("1.", false), "1.0");
        assert_eq!(format_number(".5", false), "0.5");
    }

    #[test]
    fn exponent_is_normalized_like_python() {
        assert_eq!(normalize_exponent("1e16"), "1e+16");
        assert_eq!(normalize_exponent("1e-5"), "1e-05");
        assert_eq!(normalize_exponent("1.5e-10"), "1.5e-10");
        assert_eq!(normalize_exponent("5e-324"), "5e-324");
        assert_eq!(normalize_exponent("0.0001"), "0.0001");
    }
}
