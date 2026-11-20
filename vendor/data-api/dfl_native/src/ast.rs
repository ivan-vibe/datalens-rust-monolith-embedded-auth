//! AST for the formula language, close to `dl_formula.core.nodes`.

use crate::dtype::DataType;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
    Pow,
    Like,
    NotLike,
}

impl BinOp {
    /// Name under which the operator is registered in `dl_formula`.
    pub fn name(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "==",
            BinOp::Neq => "!=",
            BinOp::Lt => "<",
            BinOp::Lte => "<=",
            BinOp::Gt => ">",
            BinOp::Gte => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
            BinOp::Pow => "^",
            BinOp::Like => "like",
            BinOp::NotLike => "notlike",
        }
    }

    /// Precedence in the formula language (used to decide where SQL needs parentheses).
    pub fn precedence(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte => 3,
            BinOp::Like | BinOp::NotLike => 3,
            BinOp::Add | BinOp::Sub => 4,
            BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
            BinOp::Pow => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
}

/// The `winGrouping` clause of a call: `TOTAL`, `AMONG <dims>` or `WITHIN <dims>`.
///
/// `Default` means no clause was written. That is *not* the same as "not a window": a call
/// to a function that cannot be used without a window (`RSUM([a])`) is a window function
/// with an empty partition in `dl_formula` terms. `analysis.rs` resolves that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Grouping {
    #[default]
    Default,
    Total,
    Among,
    Within,
}

/// The level-of-detail clause of an aggregation: `FIXED`, `INCLUDE` or `EXCLUDE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LodKind {
    Fixed,
    Include,
    Exclude,
}

#[derive(Debug, Clone)]
pub struct Lod {
    pub kind: LodKind,
    /// The dimensions of the clause; may be empty (`SUM([a] FIXED)` is legal).
    pub dims: Vec<Item>,
}

/// Direction of one `ORDER BY` item of a window function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OrderingDirection {
    #[default]
    Asc,
    Desc,
}

/// Everything that may follow the argument list of a call, in grammar order:
/// `winGrouping? ordering? lodSpecifier? beforeFilterBy? ignoreDimensions?`
///
/// `dl_formula` models all of it as separate node types hanging off `FuncCall`/`WindowFuncCall`
/// (`LodSpecifier`, `Ordering`, `BeforeFilterBy`, `IgnoreDimensions`). The native AST keeps
/// them in one struct because they are parsed in one place and always travel together.
///
/// Boxed at the use site (`Item::Call { clauses: Box<Clauses> }`) so that the size of `Item`
/// stays small: most calls have no clauses at all, but every `Item` is pushed into `Vec`s.
#[derive(Debug, Clone, Default)]
pub struct Clauses {
    pub grouping: Grouping,
    /// Dimensions of the `WITHIN`/`AMONG` clause; always empty for `TOTAL`.
    pub grouping_dims: Vec<Item>,
    pub ordering: Vec<(Item, OrderingDirection)>,
    pub lod: Option<Lod>,
    /// `BEFORE FILTER BY` names. `dl_formula` stores them as plain strings, so they are
    /// **not** part of `used_fields` and must not be reported as used fields here either.
    pub bfb: Vec<Box<str>>,
    /// The clause itself was written. The name list may be empty (`SUM([a] BEFORE FILTER BY)`),
    /// but `dl_formula` still checks that the function accepts the clause, so the presence of
    /// the clause has to be remembered separately from its contents.
    pub has_bfb: bool,
    pub ignore_dimensions: Vec<Item>,
    /// See [`Clauses::has_bfb`]: `SUM([a] IGNORE DIMENSIONS)` is rejected even with no
    /// dimensions named.
    pub has_ignore_dimensions: bool,
}

impl Clauses {
    /// No clause was written after the argument list.
    pub fn is_empty(&self) -> bool {
        self.grouping == Grouping::Default
            && self.ordering.is_empty()
            && self.lod.is_none()
            && !self.has_bfb
            && self.bfb.is_empty()
            && !self.has_ignore_dimensions
            && self.ignore_dimensions.is_empty()
    }

    /// A `TOTAL`/`AMONG`/`WITHIN` clause makes the call a `WindowFuncCall` in `dl_formula`.
    pub fn has_grouping(&self) -> bool {
        self.grouping != Grouping::Default
    }

    /// Every dimension expression any clause mentions.
    ///
    /// `dl_formula.inspect.expression.used_fields()` walks the clause nodes as well, so the
    /// fields reported by the native analysis have to include them. `BEFORE FILTER BY` is the
    /// exception: it holds field *names*, not `Field` nodes, and is therefore skipped.
    pub fn dims(&self) -> impl Iterator<Item = &Item> {
        self.grouping_dims
            .iter()
            .chain(self.ordering.iter().map(|(item, _)| item))
            .chain(self.lod.iter().flat_map(|lod| lod.dims.iter()))
            .chain(self.ignore_dimensions.iter())
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Field(Box<str>),
    /// A numeric literal with its source lexeme: `dl_formula` renders integers exactly
    /// (`int`) and floats as Python's `repr`, see `lexer::Tok::Number`.
    Num {
        text: Box<str>,
        is_int: bool,
    },
    Str(Box<str>),
    /// `#date#` / `#datetime#`: the text between the hashes, `DATE_INNER` or `DATETIME_INNER`.
    ///
    /// `dl_formula` keeps a parsed `datetime.date`/`datetime.datetime` here; the native tree
    /// keeps the source text, because the SQL literal depends on the dialect
    /// (`toDate32('2020-01-01')` for ClickHouse) and only the value check is shared.
    Date {
        text: Box<str>,
        is_datetime: bool,
    },
    Bool(bool),
    Null,
    Unary(UnOp, Box<Item>),
    Binary(BinOp, Box<Item>, Box<Item>),
    /// `exprMain [NOT] IN (a, b, ...)`.
    ///
    /// `dl_formula` puts a `nodes.ExpressionList` on the right of the `in`/`notin` binary
    /// node; `IN` is the only operator that holds one, so the list lives here directly.
    In {
        value: Box<Item>,
        list: Vec<Item>,
        negated: bool,
    },
    /// `exprMain IS [NOT] (TRUE|FALSE|NULL)`.
    Is {
        value: Box<Item>,
        kind: IsKind,
        negated: bool,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`,
    /// also the lowering of `IF cond THEN ... ELSEIF ... ELSE ... END`.
    Case {
        operand: Option<Box<Item>>,
        branches: Vec<(Item, Item)>,
        else_: Option<Box<Item>>,
    },
    /// `value [NOT] BETWEEN low AND high`
    Between {
        value: Box<Item>,
        low: Box<Item>,
        high: Box<Item>,
        negated: bool,
    },
    Call {
        name: Box<str>,
        args: Vec<Item>,
        /// `WITHIN`/`AMONG`/`TOTAL`, `ORDER BY`, LOD, `BEFORE FILTER BY`,
        /// `IGNORE DIMENSIONS` — everything after the argument list.
        clauses: Box<Clauses>,
    },
}

/// The right-hand side of `IS`: the three keywords the grammar allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsKind {
    Null,
    True,
    False,
}

/// `field name -> data type`, taken from the dataset schema.
pub type FieldTypes = HashMap<String, DataType>;

/// How a field reference is rendered in the SQL.
///
/// `dl_formula` substitutes a *node* for a field reference, so a caller may replace the
/// reference with any expression, not only with a column: `dl_query_processing` swaps a field
/// that is aggregated into the aggregate call itself (`SUM(...)`, `COUNTD(...)`), and the
/// outer formula has to render that call where the `[field]` was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldName {
    /// The physical (possibly multipart) name, quoted part by part: `["avatar", "city"]`
    /// renders as `avatar.city`.
    Path(Vec<String>),
    /// An already rendered SQL expression, emitted verbatim (e.g. `COUNT(DISTINCT t0.x)`).
    Sql(String),
}

impl From<Vec<String>> for FieldName {
    fn from(parts: Vec<String>) -> Self {
        FieldName::Path(parts)
    }
}

/// `field name -> how to render it`, e.g. `{"city": Path(["avatar", "city"])}`.
pub type FieldNames = HashMap<String, FieldName>;
