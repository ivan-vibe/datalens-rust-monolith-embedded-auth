//! What a function *is*, as opposed to how it renders in SQL.
//!
//! The validator decides whether a formula may stand in a dimension, whether it has to be
//! moved to a sub-query (`query fork`) and which clauses (`WITHIN`, LOD, `BEFORE FILTER BY`)
//! it accepts. All of those decisions start with the same three questions about a name:
//! can it be an aggregate, can it be a window function, can it be neither — and the
//! `dl_formula` answer to them lives in `LOWLEVEL_OP_REGISTRY`.
//!
//! The registry is data, so it is generated rather than ported: a generator walks the
//! live `dl_formula` registry and writes `func_props_generated.rs`. This module is the
//! typed access layer over that table and mirrors
//! `dl_formula.inspect.function.{can_be_aggregate, can_be_window, supports_bfb, ...}`.

use crate::func_props_generated::{NAME_OPS, NAME_WIN_OPS};
use std::collections::HashMap;
use std::sync::OnceLock;

pub const CAN_BE_AGGREGATE: u8 = 1;
pub const CAN_BE_WINDOW: u8 = 2;
pub const CAN_BE_NONWINDOW: u8 = 4;

pub const GROUPING: u8 = 1;
pub const ORDERING: u8 = 2;
pub const LOD: u8 = 4;
pub const IGNORE_DIMENSIONS: u8 = 8;
pub const BFB: u8 = 16;
pub const DEFAULT_ORDERING: u8 = 32;

/// `dl_formula.inspect.registry.item.NameOpItem`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NameOps {
    pub can_be_aggregate: bool,
    pub can_be_window: bool,
    pub can_be_nonwindow: bool,
}

fn names() -> &'static HashMap<&'static str, u8> {
    static TABLE: OnceLock<HashMap<&'static str, u8>> = OnceLock::new();
    TABLE.get_or_init(|| NAME_OPS.iter().copied().collect())
}

fn clauses() -> &'static HashMap<&'static str, (u8, u8)> {
    static TABLE: OnceLock<HashMap<&'static str, (u8, u8)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        NAME_WIN_OPS
            .iter()
            .map(|(name, non_window, window)| (*name, (*non_window, *window)))
            .collect()
    })
}

/// `dl_formula.inspect.registry.registry.norm_name()`: the table stores lowercase names.
fn with_lowercase<R>(name: &str, use_name: impl FnOnce(&str) -> R) -> R {
    let bytes = name.as_bytes();
    if bytes.len() <= 32 && bytes.is_ascii() {
        let mut buf = [0u8; 32];
        for (idx, byte) in bytes.iter().enumerate() {
            buf[idx] = byte.to_ascii_lowercase();
        }
        // SAFETY: `name` was checked to be ASCII and lowercasing keeps it ASCII.
        let lower = unsafe { std::str::from_utf8_unchecked(&buf[..bytes.len()]) };
        return use_name(lower);
    }
    use_name(&name.to_ascii_lowercase())
}

/// Properties of a function name, whatever the case it was written in.
pub fn name_ops(name: &str) -> NameOps {
    with_lowercase(name, |lower| match names().get(lower).copied() {
        Some(flags) => NameOps {
            can_be_aggregate: flags & CAN_BE_AGGREGATE != 0,
            can_be_window: flags & CAN_BE_WINDOW != 0,
            can_be_nonwindow: flags & CAN_BE_NONWINDOW != 0,
        },
        None => NameOps::default(),
    })
}

/// `dl_formula.inspect.function.can_be_aggregate()`
/// Everything the analysis asks about a call name, from a single table lookup.
#[derive(Debug, Clone, Copy)]
pub struct CallProps {
    /// `is_known()` — includes the `AGGR`/`NONAGGR` pseudo functions.
    pub known: bool,
    pub can_be_aggregate: bool,
    pub can_be_window: bool,
    pub can_be_nonwindow: bool,
}

/// `is_known` + `can_be_aggregate` + `can_be_window` + `can_be_nonwindow` in one hash lookup.
///
/// The analysis and the clause checks ask all of them for every call, and each of the
/// dedicated helpers lowercases the name and probes the table again, so the combined
/// accessor is what hot paths use.
pub fn call_props(name: &str) -> CallProps {
    with_lowercase(name, |lower| {
        if PSEUDO_FUNCTIONS.contains(&lower) {
            return CallProps {
                known: true,
                can_be_aggregate: false,
                can_be_window: false,
                can_be_nonwindow: true,
            };
        }
        match names().get(lower).copied() {
            Some(flags) => CallProps {
                known: true,
                can_be_aggregate: flags & CAN_BE_AGGREGATE != 0,
                can_be_window: flags & CAN_BE_WINDOW != 0,
                can_be_nonwindow: flags & CAN_BE_NONWINDOW != 0,
            },
            None => CallProps {
                known: false,
                can_be_aggregate: false,
                can_be_window: false,
                can_be_nonwindow: false,
            },
        }
    })
}

/// `dl_formula.parser.base.resolve_function_capabilities()`: is a call with these clauses a
/// `WindowFuncCall`?
///
/// A grouping clause (`TOTAL`/`AMONG`/`WITHIN`) makes a function that can be both window and
/// non-window a window one; a name that has no non-window form is a window function anyway.
/// `BEFORE FILTER BY` and the LOD clauses never change the node type.
pub fn is_window_call(name: &str, has_grouping: bool) -> bool {
    let ops = name_ops(name);
    ops.can_be_window && (!ops.can_be_nonwindow || has_grouping)
}

/// `dl_formula.inspect.function.can_be_aggregate()`
pub fn can_be_aggregate(name: &str) -> bool {
    name_ops(name).can_be_aggregate
}

/// `dl_formula.inspect.function.can_be_window()`
pub fn can_be_window(name: &str) -> bool {
    name_ops(name).can_be_window
}

/// Accepted extension clauses for `name`, either in its window or non-window form.
pub fn clause_flags(name: &str, is_window: bool) -> u8 {
    with_lowercase(name, |lower| match clauses().get(lower).copied() {
        Some((_, window)) if is_window => window,
        Some((non_window, _)) => non_window,
        None => 0,
    })
}

/// `dl_formula.inspect.function.supports_bfb()`: whether a `BEFORE FILTER BY` clause is legal.
pub fn supports_bfb(name: &str, is_window: bool) -> bool {
    clause_flags(name, is_window) & BFB != 0
}

/// `dl_formula.inspect.function.supports_grouping()`
pub fn supports_grouping(name: &str, is_window: bool) -> bool {
    clause_flags(name, is_window) & GROUPING != 0
}

/// `dl_formula.inspect.function.supports_ordering()`
pub fn supports_ordering(name: &str, is_window: bool) -> bool {
    clause_flags(name, is_window) & ORDERING != 0
}

/// `dl_formula.inspect.function.supports_ignore_dimensions()`
pub fn supports_ignore_dimensions(name: &str, is_window: bool) -> bool {
    clause_flags(name, is_window) & IGNORE_DIMENSIONS != 0
}

/// `dl_formula.inspect.function.supports_lod()`
pub fn supports_lod(name: &str, is_window: bool) -> bool {
    clause_flags(name, is_window) & LOD != 0
}

/// Whether the name can be used *without* a window clause
/// (`dl_formula`: `can_be_nonwindow`). `AGGR`/`NONAGGR` are plain wrappers, so they are
/// never window functions even though they are not in the registry.
pub fn can_be_nonwindow(name: &str) -> bool {
    with_lowercase(name, |lower| {
        if PSEUDO_FUNCTIONS.contains(&lower) {
            return true;
        }
        names()
            .get(lower)
            .is_some_and(|flags| flags & CAN_BE_NONWINDOW != 0)
    })
}

/// Names the *parser* produces without them being registered functions: `AGGR(...)` and
/// `NONAGGR(...)` are wrappers that only change the aggregation level of their argument
/// (`dl_formula` builds `FuncCall(name='aggr')` for them, the registry has no such name).
const PSEUDO_FUNCTIONS: &[&str] = &["aggr", "nonaggr"];

/// Whether the name is known to the registry at all (`False` for `PYTHON`-style plugins).
pub fn is_known(name: &str) -> bool {
    with_lowercase(name, |lower| {
        names().contains_key(lower) || PSEUDO_FUNCTIONS.contains(&lower)
    })
}

/// Number of distinct function/operator names in the generated table.
pub fn len() -> usize {
    NAME_OPS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_generated_and_matches_dl_formula() {
        assert!(len() > 200, "the generated table looks empty: {}", len());
        // `dl_formula.inspect.function.can_be_aggregate("sum") == True`
        assert!(can_be_aggregate("sum"));
        assert!(can_be_aggregate("SUM"));
        // aggregates are also window functions (`SUM(...) WITHIN ...`)
        assert!(can_be_window("sum"));
        // `rank` is window-only
        assert!(can_be_window("rank"));
        assert!(!can_be_aggregate("rank"));
        // `aggr`/`nonaggr` wrap an expression, they are not aggregates themselves
        assert!(!can_be_aggregate("aggr"));
        assert!(!can_be_aggregate("nonaggr"));
        // plain functions are neither
        assert!(!can_be_aggregate("upper"));
        assert!(!can_be_window("upper"));
        assert!(is_known("upper"));
        assert!(!is_known("definitely_not_a_function"));
    }

    #[test]
    fn extension_clauses_come_from_the_same_table() {
        // `dl_formula.inspect.function.supports_bfb("avg", is_window=False) == True`
        assert!(supports_bfb("avg", false));
        assert!(!supports_bfb("upper", false));
        // LOD is accepted by the *non-window* form of `sum` (`SUM([a] FIXED [b])`)
        assert!(supports_lod("sum", false));
        assert!(!supports_lod("sum", true));
    }
}
