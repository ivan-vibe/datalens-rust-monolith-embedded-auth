//! Structural analysis of a parsed formula, mirroring `dl_formula.inspect.expression`.
//!
//! This is the first half of the `validate` / `query_forks` work: before a field can be
//! validated or moved into a sub-query, the engine has to know
//!
//! * whether the expression contains an aggregation or a window function,
//! * how deeply aggregations are nested (`SUM(SUM(...))` is what forces a query fork),
//! * which functions are used at all, and which of them the registry does not know,
//! * whether the expression is constant (no fields, no aggregates, no windows).
//!
//! Python answers those with a recursive walk over `autonomous_children()` and
//! `dl_formula.inspect.node.is_aggregate_function()`. The walk is a pure function of the
//! AST, so it is a natural candidate for the native engine — and, unlike the SQL
//! translation, it does not need a dialect at all.
//!
//! Two things the native AST cannot express yet are reported as `false`/`0` on purpose
//! (measured while porting):
//!   * `QueryFork` nodes — they only exist after `dl_query_processing` mutations,
//!     never in the formula text a request contains,
//!   * `AMONG` groupings and LOD windows are counted as windows/aggregates, but
//!     `query_fork_nesting` stays 0 because the fork level is decided by the caller.
//!
//! The clause grammar (`WITHIN`/`AMONG`/`TOTAL`, `ORDER BY`, `FIXED`/`INCLUDE`/`EXCLUDE`,
//! `BEFORE FILTER BY`, `IGNORE DIMENSIONS`) is fully parsed (see `parser.rs`), so the
//! analysis does not fall back to Python for formulas that use it.

use crate::ast::{Clauses, Item};
use crate::func_props::{self, CallProps};

/// Everything the validator needs to know about one formula, in one pass over the AST.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// Any `SUM`/`AVG`/... that is not part of a window call.
    pub has_aggregate: bool,
    /// Any call with a `WITHIN` clause (a `WindowFuncCall` in `dl_formula` terms).
    pub has_window: bool,
    /// Maximum number of aggregations on a single root-to-leaf path.
    pub max_aggregate_nesting: u8,
    /// Maximum number of window functions on a single root-to-leaf path.
    pub max_window_nesting: u8,
    /// `dl_formula.inspect.expression.is_constant_expression()`.
    pub is_constant: bool,
    /// `dl_formula.inspect.expression.get_query_fork_nesting_level()` — never > 0 here.
    pub query_fork_nesting: u8,
    /// A non-empty `BEFORE FILTER BY` clause was seen (`node.before_filter_by.field_names`).
    pub has_bfb: bool,
    /// A level-of-detail clause (`FIXED`/`INCLUDE`/`EXCLUDE`) was seen.
    pub has_lod: bool,
    /// `dl_formula.inspect.node.is_extended_aggregation()`: an aggregation with a custom LOD
    /// or a `BEFORE FILTER BY` filter — `dl_query_processing` moves it into a sub-query.
    pub has_extended_aggregation: bool,
    /// Distinct field names, in order of first appearance.
    pub fields: Vec<String>,
    /// Distinct aggregate names (`sum`, `avg`, ...).
    pub aggregates: Vec<String>,
    /// Distinct window function names.
    pub windows: Vec<String>,
    /// Distinct function names the generated table does not know — a validation signal.
    pub unknown_functions: Vec<String>,
    /// Total number of function/operator calls, a cheap measure of expression size.
    pub call_count: u32,
}

fn push_unique(target: &mut Vec<String>, value: &str) {
    if !target.iter().any(|existing| existing == value) {
        target.push(value.to_string());
    }
}

/// Whether a call is a `WindowFuncCall` in `dl_formula` terms.
///
/// The node type is decided by the parser (`dl_formula.parser.base.resolve_function_capabilities`):
/// a grouping clause (`TOTAL`/`AMONG`/`WITHIN`) makes the call a window, and so does a name
/// that has no non-window form at all (`RSUM([a])` parses as a window with a `TOTAL` grouping).
/// `BEFORE FILTER BY` and LOD clauses do not change the node type.
///
/// `func_props::is_window_call()` is the same rule for callers that only have the name.
fn is_window_call(props: &CallProps, clauses: &Clauses) -> bool {
    props.can_be_window && (!props.can_be_nonwindow || clauses.has_grouping())
}

/// Function names are compared case-insensitively, like `dl_formula`'s `norm_name()` does.
fn push_unique_lower(target: &mut Vec<String>, value: &str) {
    let lower = value.to_ascii_lowercase();
    if !target.iter().any(|existing| existing == &lower) {
        target.push(lower);
    }
}

impl Analysis {
    /// `dl_formula.inspect.expression.is_constant_expression()`
    pub fn is_constant_expression(item: &Item) -> bool {
        match item {
            Item::Num { .. } | Item::Str(_) | Item::Bool(_) | Item::Null | Item::Date { .. } => {
                true
            }
            Item::Field(_) => false,
            Item::Unary(_, child) => Self::is_constant_expression(child),
            Item::Binary(_, left, right) => {
                Self::is_constant_expression(left) && Self::is_constant_expression(right)
            }
            Item::Between {
                value, low, high, ..
            } => {
                Self::is_constant_expression(value)
                    && Self::is_constant_expression(low)
                    && Self::is_constant_expression(high)
            }
            Item::Is { value, .. } => Self::is_constant_expression(value),
            Item::In { value, list, .. } => {
                Self::is_constant_expression(value) && list.iter().all(Self::is_constant_expression)
            }
            Item::Case {
                operand,
                branches,
                else_,
            } => {
                operand
                    .as_deref()
                    .map_or(true, Self::is_constant_expression)
                    && branches.iter().all(|(when, then)| {
                        Self::is_constant_expression(when) && Self::is_constant_expression(then)
                    })
                    && else_.as_deref().map_or(true, Self::is_constant_expression)
            }
            Item::Call {
                name,
                args,
                clauses,
            } => {
                // `count(1)` looks constant but is not: aggregates and windows are evaluated
                // over a group/partition, exactly like in `dl_formula.inspect.expression`.
                let props = func_props::call_props(name);
                if is_window_call(&props, clauses) || props.can_be_aggregate {
                    false
                } else {
                    args.iter().all(Self::is_constant_expression)
                }
            }
        }
    }

    /// The `dfl_analyze` payload: one JSON object per formula, no `serde` dependency.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("{\"ok\":true");
        out.push_str(&format!(",\"has_aggregate\":{}", self.has_aggregate));
        out.push_str(&format!(",\"has_window\":{}", self.has_window));
        out.push_str(&format!(",\"is_constant\":{}", self.is_constant));
        out.push_str(&format!(",\"has_bfb\":{}", self.has_bfb));
        out.push_str(&format!(",\"has_lod\":{}", self.has_lod));
        out.push_str(&format!(
            ",\"has_extended_aggregation\":{}",
            self.has_extended_aggregation
        ));
        out.push_str(&format!(
            ",\"max_aggregate_nesting\":{}",
            self.max_aggregate_nesting
        ));
        out.push_str(&format!(
            ",\"max_window_nesting\":{}",
            self.max_window_nesting
        ));
        out.push_str(&format!(
            ",\"query_fork_nesting\":{}",
            self.query_fork_nesting
        ));
        out.push_str(&format!(",\"call_count\":{}", self.call_count));
        for (key, values) in [
            ("fields", &self.fields),
            ("aggregates", &self.aggregates),
            ("windows", &self.windows),
            ("unknown_functions", &self.unknown_functions),
        ] {
            out.push_str(&format!(",\"{key}\":["));
            for (idx, value) in values.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&json_escape(value));
                out.push('"');
            }
            out.push(']');
        }
        out.push('}');
        out
    }
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

struct Walker<'a> {
    analysis: &'a mut Analysis,
    aggregate_depth: u8,
    window_depth: u8,
}

impl Walker<'_> {
    fn visit(&mut self, item: &Item) {
        match item {
            Item::Field(name) => push_unique(&mut self.analysis.fields, name),
            Item::Num { .. } | Item::Str(_) | Item::Bool(_) | Item::Null | Item::Date { .. } => {}
            Item::Unary(_, child) => self.visit(child),
            Item::Binary(_, left, right) => {
                self.visit(left);
                self.visit(right);
            }
            Item::Between {
                value, low, high, ..
            } => {
                self.visit(value);
                self.visit(low);
                self.visit(high);
            }
            Item::Is { value, .. } => self.visit(value),
            Item::In { value, list, .. } => {
                self.visit(value);
                for item in list {
                    self.visit(item);
                }
            }
            Item::Case {
                operand,
                branches,
                else_,
            } => {
                if let Some(operand) = operand.as_deref() {
                    self.visit(operand);
                }
                for (when, then) in branches {
                    self.visit(when);
                    self.visit(then);
                }
                if let Some(else_) = else_.as_deref() {
                    self.visit(else_);
                }
            }
            Item::Call {
                name,
                args,
                clauses,
            } => {
                self.analysis.call_count += 1;
                let props = func_props::call_props(name);
                if !props.known {
                    push_unique(&mut self.analysis.unknown_functions, name);
                }
                if !clauses.bfb.is_empty() {
                    self.analysis.has_bfb = true;
                }
                if clauses.lod.is_some() {
                    self.analysis.has_lod = true;
                }
                // `dl_formula` keeps window calls as a separate node type: a call with a
                // grouping clause (`TOTAL`/`AMONG`/`WITHIN`) is a window function, and so is a
                // name that cannot be used without one (`RSUM([a])` is a `WindowFuncCall` with
                // no grouping at all). LOD and `BEFORE FILTER BY` do not change the node type.
                let is_window = is_window_call(&props, clauses);
                let is_aggregate = !is_window && props.can_be_aggregate;
                if is_aggregate && (clauses.lod.is_some() || !clauses.bfb.is_empty()) {
                    // `dl_formula.inspect.node.is_extended_aggregation()`
                    self.analysis.has_extended_aggregation = true;
                }
                if is_window {
                    self.analysis.has_window = true;
                    push_unique_lower(&mut self.analysis.windows, name);
                    self.window_depth += 1;
                    self.analysis.max_window_nesting =
                        self.analysis.max_window_nesting.max(self.window_depth);
                } else if is_aggregate {
                    self.analysis.has_aggregate = true;
                    push_unique_lower(&mut self.analysis.aggregates, name);
                    self.aggregate_depth += 1;
                    self.analysis.max_aggregate_nesting = self
                        .analysis
                        .max_aggregate_nesting
                        .max(self.aggregate_depth);
                }
                for arg in args {
                    self.visit(arg);
                }
                // Clause dimensions are ordinary children in `dl_formula`
                // (`FuncCall.children == args + lod + ignore_dimensions + before_filter_by`,
                // `WindowFuncCall` adds `grouping` and `ordering`), so fields inside them are
                // used fields and nested aggregations inside them count towards the nesting.
                for dimension in clauses.dims() {
                    self.visit(dimension);
                }
                if is_window {
                    self.window_depth -= 1;
                } else if is_aggregate {
                    self.aggregate_depth -= 1;
                }
            }
        }
    }
}

/// One pass over the AST; the analogue of the several `dl_formula.inspect.expression`
/// walks the validator performs today (each of them re-enumerates the same children).
pub fn analyze(item: &Item) -> Analysis {
    let mut analysis = Analysis {
        is_constant: Analysis::is_constant_expression(item),
        ..Analysis::default()
    };
    let mut walker = Walker {
        analysis: &mut analysis,
        aggregate_depth: 0,
        window_depth: 0,
    };
    walker.visit(item);
    analysis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_formula;

    fn analyze_text(text: &str) -> Analysis {
        analyze(&parse_formula(text).unwrap_or_else(|err| panic!("cannot parse {text}: {err}")))
    }

    #[test]
    fn plain_expression_is_neither_aggregate_nor_window() {
        let analysis = analyze_text("[a] + 1");
        assert!(!analysis.has_aggregate);
        assert!(!analysis.has_window);
        assert!(!analysis.is_constant);
        assert_eq!(analysis.fields, vec!["a".to_string()]);
        assert_eq!(analysis.max_aggregate_nesting, 0);
    }

    #[test]
    fn nested_aggregates_are_counted() {
        assert_eq!(analyze_text("SUM([a])").max_aggregate_nesting, 1);
        // `SUM(SUM([a]))` is the classic query-fork trigger
        assert_eq!(analyze_text("SUM(SUM([a]))").max_aggregate_nesting, 2);
        assert_eq!(
            analyze_text("AVG(SUM([a])) + SUM([b])").max_aggregate_nesting,
            2
        );
        assert_eq!(analyze_text("SUM([a]) + SUM([b])").max_aggregate_nesting, 1);
    }

    #[test]
    fn within_marks_a_call_as_window_function() {
        let analysis = analyze_text("RSUM([a] WITHIN [b])");
        assert!(analysis.has_window);
        assert!(
            !analysis.has_aggregate,
            "a window of a non-aggregate stays a window"
        );
        assert_eq!(analysis.windows, vec!["rsum".to_string()]);
        assert_eq!(analysis.max_window_nesting, 1);
    }

    #[test]
    fn window_only_names_are_windows_even_without_a_clause() {
        // `dl_formula` parses these as `WindowFuncCall` with `WindowGroupingTotal`
        assert!(analyze_text("RSUM([a])").has_window);
        assert!(analyze_text("RANK([a])").has_window);
        // ... while an aggregate without a clause stays an aggregate
        assert!(!analyze_text("SUM([a])").has_window);
        assert!(analyze_text("SUM([a])").has_aggregate);
    }

    #[test]
    fn within_binds_to_the_innermost_open_function() {
        // `RSUM(SUM([a]) WITHIN [b])`: the window is the outer call, `SUM` stays an aggregate
        let outer = analyze_text("RSUM(SUM([a]) WITHIN [b])");
        assert!(outer.has_window);
        assert!(outer.has_aggregate);
        assert_eq!(outer.aggregates, vec!["sum".to_string()]);
        assert_eq!(outer.max_aggregate_nesting, 1);
        // `RSUM(AVG([a] WITHIN [b]))`: the window is the inner call
        let inner = analyze_text("RSUM(AVG([a] WITHIN [b]))");
        assert!(inner.has_window);
        assert!(!inner.has_aggregate);
    }

    #[test]
    fn constant_expressions_have_no_fields_and_no_aggregates() {
        assert!(analyze_text("1 + 2").is_constant);
        assert!(!analyze_text("[a] * 2").is_constant);
        // `count(1)` is not constant: it aggregates over the query result
        assert!(!analyze_text("COUNT(1)").is_constant);
        assert!(!analyze_text("RANK([a] WITHIN [b])").is_constant);
    }

    #[test]
    fn unknown_functions_are_reported() {
        let analysis = analyze_text("SOME_PLUGIN_FN([a])");
        assert_eq!(
            analysis.unknown_functions,
            vec!["SOME_PLUGIN_FN".to_string()]
        );
        // `aggr`/`nonaggr` are known wrappers and not aggregates
        let wrapper = analyze_text("AGGR(SUM([a]))");
        assert!(wrapper.unknown_functions.is_empty());
        assert!(wrapper.has_aggregate);
        assert_eq!(wrapper.max_aggregate_nesting, 1);
    }

    #[test]
    fn extension_clauses_are_parsed_and_analysed() {
        // `BEFORE FILTER BY` holds field *names*, so it filters the formula without
        // contributing to `used_fields` — exactly like `BeforeFilterBy.field_names` in Python.
        let bfb = analyze_text("SUM([a] BEFORE FILTER BY [b])");
        assert!(bfb.has_aggregate);
        assert!(bfb.has_bfb);
        assert!(bfb.has_extended_aggregation);
        assert_eq!(bfb.fields, vec!["a".to_string()]);
        // ... and an empty `BEFORE FILTER BY` is not a filter (the frozenset is empty)
        assert!(!analyze_text("SUM([a] BEFORE FILTER BY)").has_bfb);

        // LOD dimensions *are* used fields, and a LOD aggregation is an extended aggregation
        let lod = analyze_text("SUM([a] FIXED [b])");
        assert!(lod.has_lod);
        assert!(lod.has_extended_aggregation);
        assert_eq!(lod.fields, vec!["a".to_string(), "b".to_string()]);
        assert!(!analyze_text("SUM([a])").has_lod);
        assert!(!analyze_text("SUM([a])").has_extended_aggregation);

        // `ORDER BY` / `IGNORE DIMENSIONS` dimensions are children of the call as well
        let window = analyze_text("RSUM([a] WITHIN [b] ORDER BY [c] DESC)");
        assert!(window.has_window);
        assert!(!window.has_aggregate);
        assert_eq!(
            window.fields,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            analyze_text("AGO([a], [b], [c] IGNORE DIMENSIONS [d])").fields,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        );
    }

    #[test]
    fn grouping_clauses_turn_an_aggregate_into_a_window() {
        // `SUM([a] TOTAL)` is a `WindowFuncCall` in `dl_formula`, not an aggregate
        for formula in [
            "SUM([a] TOTAL)",
            "SUM([a] AMONG [b])",
            "SUM([a] WITHIN [b])",
        ] {
            let analysis = analyze_text(formula);
            assert!(analysis.has_window, "{formula}");
            assert!(!analysis.has_aggregate, "{formula}");
            assert_eq!(analysis.windows, vec!["sum".to_string()], "{formula}");
        }
        // a LOD clause does not make a window out of an aggregate
        let lod = analyze_text("SUM([a] FIXED [b])");
        assert!(!lod.has_window);
        assert!(lod.has_aggregate);
    }
}
