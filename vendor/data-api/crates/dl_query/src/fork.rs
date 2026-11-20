//! `dl_query_processing.multi_query.splitters.query_fork.QueryForkQuerySplitter`.
//!
//! A formula comes in as a template over leaves ([`dfl_native::Leaf`]). Leaves that carry a LOD
//! clause cannot be computed by the query that reads the rows — `COUNTD([user] FIXED [category])`
//! counts over a whole category — so each one becomes a query of its own, grouped by the LOD
//! dimensions, and the base query joins it back:
//!
//! ```sql
//! SELECT q_0.e_0, q_1.e_1 / q_0.e_2
//! FROM (SELECT t0."category" AS e_0, count(DISTINCT t0."user") AS e_2
//!       FROM orders t0 GROUP BY t0."category") q_0
//! CROSS JOIN (SELECT count(DISTINCT t0."user") AS e_1 FROM orders t0) q_1
//! ```
//!
//! A fork with a dimension key joins with `nulls_equal` on that key; a fork without dimensions
//! (`TOTAL`, `FIXED` with no dimensions) is exactly one row and joins as a `CROSS JOIN` — the
//! constant key Python writes (`1 AS e_N`, `ON (1 = 1)`-style condition) is what made
//! PostgreSQL re-execute the fork for every row of the base.
//!
//! Names `e_N` are allocated once for the whole plan and `q_N` in fork order, with `q_0` for the
//! base — the numbering `QueryForkQuerySplitter` produces.

use dfl_native::{ExternalCall, Leaf};

use crate::literal::Dialect;
use crate::plan::{Cte, Join, Select, SelectExpr, Source};
use crate::QueryError;

/// A request filter built on a window function, e.g. `RANK([Sales]) <= 5`.
///
/// It belongs to neither `WHERE` (the window is computed after the grouping) nor `HAVING` (no
/// dialect takes a window there), so the plan puts the window in the query below — as a column
/// the request does not have to select — and filters the query above it.
pub struct WindowFilter {
    /// The field's expression, as the `SELECT` list spells it (`{leaf:N}` or SQL).
    pub expr: String,
    /// The column the expression gets in the query below.
    pub alias: String,
    /// The condition, still written in terms of `expr`.
    pub condition: String,
}

/// Build the plan for a request whose formulas fork.
///
/// `base` is the query the compiler would have emitted, with `{leaf:N}` left in its item
/// expressions; `leaves` are those leaves, in the same numbering.
pub fn split(
    base: Select,
    leaves: &[Leaf],
    window_filters: &[WindowFilter],
    dialect: Dialect,
) -> Result<Select, QueryError> {
    let forks: usize = leaves.iter().filter(|leaf| leaf.external.is_some()).count();
    if forks == 0 {
        // No fork: the formula is its own SQL, and the plan is the single query. Every place a
        // leaf was lifted from gets it back, so the SQL is the one `compile_named` built.
        let select = Select {
            items: base
                .items
                .iter()
                .map(|item| SelectExpr {
                    sql: inline(&item.sql, leaves),
                    alias: item.alias.clone(),
                })
                .collect(),
            filters: base.filters.iter().map(|sql| inline(sql, leaves)).collect(),
            group_by: base
                .group_by
                .iter()
                .map(|sql| inline(sql, leaves))
                .collect(),
            having: base.having.iter().map(|sql| inline(sql, leaves)).collect(),
            order_by: base
                .order_by
                .iter()
                .map(|sql| inline(sql, leaves))
                .collect(),
            ..base
        };
        if window_filters.is_empty() {
            return Ok(select);
        }
        return Ok(filter_windows(
            add_window_columns(select, window_filters, leaves),
            window_filters,
        ));
    }

    let mut names = Names { next: 0 };
    // `QueryForkQuerySplitter.get_split_masks`: a filter built *on* a forked expression cannot
    // be applied at this level — the pieces it is built from are selected by other queries, and
    // a window function is not allowed in `HAVING` — so Python drops it from every query here.
    // `RANK([Sales]) <= 5` is such a filter: the reference answers with all the rows.
    let forked = |template: &str| {
        leaves
            .iter()
            .enumerate()
            .any(|(index, leaf)| leaf.external.is_some() && mentions(template, index))
    };
    let mut filters: Vec<String> = Vec::with_capacity(base.filters.len());
    for filter in &base.filters {
        // The placeholder is checked before it is inlined: after `inline` the filter is the
        // raw expression of the fork, which no longer says that it was forked.
        if forked(filter) {
            continue;
        }
        filters.push(inline(filter, leaves));
    }
    let mut having: Vec<String> = Vec::with_capacity(base.having.len());
    for condition in &base.having {
        if forked(condition) {
            continue;
        }
        having.push(inline(condition, leaves));
    }
    // A leaf the plan never mentions is not a column of the base query: a dimension's formula
    // may lift the fields it is built from before [`crate::compiler`] folds it back into one
    // expression, and the leftover leaves must not become `e_N` columns nothing selects.
    let used = used_leaves(&base, leaves);
    // A leaf that is the whole select item, or that aggregates, is a column of the base query:
    // the outer formula refers to it as `q_0.e_N` (an aggregate cannot be recomputed above the
    // grouping). A *nested* leaf that does not aggregate is not: `Datetrunc([Order date],
    // "month")` groups by the cast, so selecting the raw date as well would either break the
    // `GROUP BY` or change the grain of the query. It is inlined back instead — both in the base
    // item it is part of and in the outer template ([`place`]).
    let top_level = top_level_leaves(&base, leaves);
    // An aggregate *over* a fork (`AVG(SUM([Sales] INCLUDE [Region]))`) is not a column of the
    // base query either: it aggregates the rows the fork produced, so it is computed in a query
    // above the fork — one level per fork, the way `QueryForkQuerySplitter` nests its masks.
    let globals: Vec<String> = base
        .group_by
        .iter()
        .map(|sql| inline(sql, leaves))
        .collect();
    // The dimensions of a fork that the request groups by, in the order the request has them:
    // the level above a fork is grouped by exactly these.
    let fork_globals = |leaf: &Leaf| -> Vec<String> {
        let Some(external) = &leaf.external else {
            return Vec::new();
        };
        // A lookup fork and an `INCLUDE` fork group by the request's dimensions as well, so
        // every one of them is a column the level above can join on.
        match (&external.lookup, &external.kind) {
            (Some(_), _) => globals.clone(),
            (None, dfl_native::ExternalKind::Lod(dfl_native::LodKind::Include)) => globals.clone(),
            (None, _) => globals
                .iter()
                .filter(|dimension| {
                    external
                        .dimensions
                        .iter()
                        .any(|own| &inline(own, leaves) == *dimension)
                })
                .cloned()
                .collect(),
        }
    };
    // `(dependent leaf, the fork it is built from)`.
    let mut dependent: Vec<(usize, usize)> = Vec::new();
    for (index, leaf) in leaves.iter().enumerate() {
        if !used[index] || leaf.external.is_some() || !leaf.aggregated {
            continue;
        }
        let forks: Vec<usize> = (0..leaves.len())
            .filter(|other| leaves[*other].external.is_some() && mentions(&leaf.sql, *other))
            .collect();
        if forks.len() == 1 && !fork_globals(&leaves[forks[0]]).is_empty() {
            dependent.push((index, forks[0]));
        }
    }
    // A window the request *filters* on is a column of the query above the joins: the condition
    // compares the column by its alias, and `WHERE` cannot see the alias of the `SELECT` that
    // computes it. A leaf placeholder can be placed there (`dense_rank() OVER (ORDER BY
    // q_1.e_5)`); an expression that is not one has to stay in the base query.
    let filtered_windows: Vec<Option<usize>> = window_filters
        .iter()
        .map(|filter| single_leaf(&filter.expr))
        .collect();
    // Base columns: every leaf that is not forked becomes `e_N` of the base query, and the
    // template refers to it as `q_0.e_N`.
    //
    // A base item is one expression over the source, so the leaves the base computes itself are
    // put back as their SQL. A leaf a *fork* computes keeps its placeholder instead: the fork's
    // column is part of the base query, the expression behind it is not — `max(q_1.e_3) OVER
    // (...)` is one window, `max(max(...) OVER (...)) OVER (...)` is none. The placeholder is
    // filled in by [`place_forks`] once the forks are built.
    let inline_base = |template: &str| -> String {
        let mut sql = template.to_string();
        loop {
            let mut replaced = false;
            for (index, leaf) in leaves.iter().enumerate() {
                let token = format!("{{leaf:{index}}}");
                if leaf.external.is_some() || !sql.contains(&token) || leaf.sql.contains(&token) {
                    continue;
                }
                sql = sql.replace(&token, &leaf.sql);
                replaced = true;
            }
            if !replaced {
                return sql;
            }
        }
    };
    let mut base_items: Vec<SelectExpr> = Vec::new();
    let mut base_names: Vec<Option<String>> = vec![None; leaves.len()];
    for (index, leaf) in leaves.iter().enumerate() {
        if leaf.external.is_some() || !used[index] {
            continue;
        }
        // A window the request filters on is computed above the joins, not here.
        if filtered_windows.contains(&Some(index)) {
            continue;
        }
        if !leaf.aggregated && !top_level[index] {
            continue;
        }
        if dependent.iter().any(|(index_, _)| *index_ == index) {
            continue;
        }
        let name = names.take();
        base_items.push(SelectExpr {
            // A leaf that is itself a template over other leaves (`AVG({leaf:1}) OVER (...)`)
            // selects the whole expression: a query cannot refer to its own column alias, so
            // the nested leaves are inlined back.
            sql: inline_base(&leaf.sql),
            alias: name.clone(),
        });
        base_names[index] = Some(name);
    }

    let mut joins = Vec::with_capacity(forks);
    // A fork's column, by leaf index: what the template refers to as `q_k.e_N`.
    let mut fork_names: Vec<Option<(String, String)>> = vec![None; leaves.len()];
    // What a query above a fork needs: the fork itself as a source, its value column and the
    // columns of the dimensions the request groups by.
    #[derive(Clone)]
    struct ForkColumns {
        select: Select,
        value: String,
        keys: Vec<(String, String, String)>,
    }
    let mut fork_columns: Vec<Option<ForkColumns>> = vec![None; leaves.len()];
    let mut group_by = base.group_by.clone();
    // A fork that only feeds the query *above* it (`AVG(SUM([Sales] INCLUDE [Region]))`) is not
    // joined to the base at all: the level above reads it as its own source, and joining it here
    // as well would cut the base to the fork's grain — one row per region instead of per month.
    let direct_use = |fork_index: usize| -> bool {
        let mentioned = |sql: &str| mentions(sql, fork_index);
        base.items
            .iter()
            .map(|item| &item.sql)
            .chain(&base.filters)
            .chain(&base.group_by)
            .chain(&base.having)
            .chain(&base.order_by)
            .any(|sql| mentioned(sql))
            || leaves.iter().enumerate().any(|(index, leaf)| {
                used[index]
                    && !dependent.iter().any(|(leaf, _)| *leaf == index)
                    && mentioned(&leaf.sql)
            })
    };
    for (fork_index, leaf) in leaves.iter().enumerate() {
        let Some(external) = &leaf.external else {
            continue;
        };
        let alias = format!("q_{}", joins.len() + 1);
        // The call and its dimensions may refer to lifted leaves (`COUNTD([Region] FIXED)`):
        // inside a fork they are plain expressions over the same source, not columns of `q_0`.
        let call = inline(&external.sql, leaves);
        // `AVG([Sales] TOTAL)` is the average of the *grouped* sums: the fork becomes two
        // queries — one that aggregates per request dimension, one that aggregates those
        // values — and the base joins the single row it returns by a constant key. The
        // distributive aggregates keep the one-query shape: for them both plans are the same
        // number.
        if let Some((_, argument)) = nested_total_call(external, &call, &globals) {
            let alias = format!("q_{}", joins.len() + 1);
            let inner_alias = format!("f_{}", joins.len());
            let mut inner_items: Vec<SelectExpr> = Vec::with_capacity(globals.len() + 1);
            let mut inner_group_by: Vec<String> = Vec::with_capacity(globals.len());
            for dimension in &globals {
                let name = names.take();
                inner_items.push(SelectExpr {
                    sql: dimension.clone(),
                    alias: name,
                });
                inner_group_by.push(dimension.clone());
            }
            let inner_value = names.take();
            inner_items.push(SelectExpr {
                sql: argument.to_string(),
                alias: inner_value.clone(),
            });
            let inner = Select {
                items: inner_items,
                from: base.from.clone(),
                joins: Vec::new(),
                filters: filters.clone(),
                group_by: inner_group_by,
                having: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                ctes: Vec::new(),
            };
            let base_key = names.take();
            base_items.push(SelectExpr {
                sql: "1".to_string(),
                alias: base_key.clone(),
            });
            let fork_key = names.take();
            let value_name = names.take();
            let outer_call = call.replacen(argument, &format!("{inner_alias}.{inner_value}"), 1);
            let select = Select {
                items: vec![
                    SelectExpr {
                        sql: "1".to_string(),
                        alias: fork_key.clone(),
                    },
                    SelectExpr {
                        sql: outer_call,
                        alias: value_name.clone(),
                    },
                ],
                from: Source::Subquery {
                    select: Box::new(inner),
                    alias: inner_alias,
                },
                joins: Vec::new(),
                filters: Vec::new(),
                group_by: Vec::new(),
                having: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                ctes: Vec::new(),
            };
            fork_columns[fork_index] = Some(ForkColumns {
                select: select.clone(),
                value: value_name.clone(),
                keys: Vec::new(),
            });
            // The base formula refers to the fork by `q_N.e_M`; without this entry the leaf is
            // put back as its SQL (`avg(q_0.e_1)`), which is not a valid aggregate over the
            // joined query.
            fork_names[fork_index] = Some((alias.clone(), value_name));
            joins.push(Join {
                source: Source::Subquery {
                    select: Box::new(select),
                    alias: alias.clone(),
                },
                // The fork is one row (`avg` over the grouped rows) and both sides name the key
                // as the constant `1`: there is no column to match, so the relation is a
                // `CROSS JOIN`. Python spells it `ON (q_0.e_5 = q_1.e_6 OR (... IS NULL ...))`,
                // which PostgreSQL cannot fold into a tautology — it re-runs the fork for every
                // row of the base (`Nested Loop loops=8146`, 8-10 s on the drill-down table).
                // The constants stay in the `SELECT` lists, so only the join type changes.
                on: Vec::new(),
                left: false,
            });
            continue;
        }
        // `LookupFunctionToQueryForkMutation` joins a copy of the query grouped by *all* the
        // dimensions of the request, not just the lookup one: the offset replaces the match on
        // the lookup dimension, and every other dimension has to match exactly — otherwise the
        // previous month of one category is joined to the current month of another.
        let lookup = external.lookup.as_ref();
        let lookup_dimension = lookup.map(|lookup| inline(&lookup.dimension, leaves));
        let mut dimensions: Vec<String> = match &lookup_dimension {
            Some(_) => globals.clone(),
            None => external
                .dimensions
                .iter()
                .map(|dimension| inline(dimension, leaves))
                .collect(),
        };
        // `INCLUDE` only *adds* a dimension to the grouping — `SUM([Sales] INCLUDE [Region])`
        // sums per region *and* per month — while `FIXED` and `TOTAL` replace it.
        if matches!(
            external.kind,
            dfl_native::ExternalKind::Lod(dfl_native::LodKind::Include)
        ) {
            let mut grouping = globals.clone();
            for dimension in dimensions {
                if !grouping.contains(&dimension) {
                    grouping.push(dimension);
                }
            }
            dimensions = grouping;
        }
        let joined_to_base = !dependent.iter().any(|(_, fork)| *fork == fork_index)
            || direct_use(fork_index)
            // Without a dimension there is no key the level above could join on, so the fork
            // keeps its constant key and the base.
            || dimensions.is_empty();
        if lookup.is_some() && dimensions.is_empty() {
            return Err(QueryError::Invalid(
                "the dimension of a lookup (AGO/AT_DATE) has to be a dimension of the request"
                    .to_string(),
            ));
        }
        let mut joined = false;
        let mut dimension_items: Vec<SelectExpr> = Vec::with_capacity(dimensions.len());
        let mut keys: Vec<(String, String, String)> = Vec::new();
        let mut fork_group_by: Vec<String> = Vec::with_capacity(dimensions.len());
        let mut on: Vec<String> = Vec::with_capacity(dimensions.len());
        for dimension in &dimensions {
            // The base has to select the dimension too: it is the join key.
            let base_name = match base_items.iter().find(|item| &item.sql == dimension) {
                Some(item) => item.alias.clone(),
                None if !joined_to_base => String::new(),
                None => {
                    let name = names.take();
                    if !group_by.iter().any(|expr| expr == dimension) {
                        group_by.push(dimension.clone());
                    }
                    base_items.push(SelectExpr {
                        sql: dimension.clone(),
                        alias: name.clone(),
                    });
                    name
                }
            };
            let fork_name = names.take();
            dimension_items.push(SelectExpr {
                sql: dimension.clone(),
                alias: fork_name.clone(),
            });
            fork_group_by.push(dimension.clone());
            if fork_globals(leaf).contains(dimension) {
                keys.push((dimension.clone(), base_name.clone(), fork_name.clone()));
            }
            match (&lookup, &lookup_dimension) {
                // The base side is the dimension itself for `AGO` (`{dim}`) and the date
                // expression for `AT_DATE`; the fork side is the offset or the bare dimension.
                (Some(lookup), Some(lookup_dimension)) if dimension == lookup_dimension => {
                    let column = format!("{alias}.{fork_name}");
                    on.push(nulls_equal_expr(
                        &inline(&lookup.base, leaves).replace("{dim}", &format!("q_0.{base_name}")),
                        &lookup.offset.replace("{dim}", &column),
                    ));
                    if let Some(day) = &lookup.day {
                        on.push(nulls_equal_expr(
                            &day.replace("{dim}", &format!("q_0.{base_name}")),
                            &day.replace("{dim}", &column),
                        ));
                    }
                    joined = true;
                }
                _ if joined_to_base => on.push(nulls_equal("q_0", &base_name, &alias, &fork_name)),
                _ => {}
            }
        }
        if lookup.is_some() && !joined {
            return Err(QueryError::Invalid(
                "the dimension of a lookup (AGO/AT_DATE) has to be a dimension of the request"
                    .to_string(),
            ));
        }
        let mut constant_key: Option<String> = None;
        if dimensions.is_empty() {
            // An ungrouped LOD (`FIXED` without dimensions, `TOTAL`) is one row: the only key it
            // could be joined by is a constant. The constants stay selected (that is what Python
            // emits), but the join itself is a `CROSS JOIN` — matching on the constant makes
            // PostgreSQL re-run the whole fork once per row of the base.
            let base_name = names.take();
            base_items.push(SelectExpr {
                sql: "1".to_string(),
                alias: base_name.clone(),
            });
            constant_key = Some(names.take());
        }
        // `BEFORE FILTER BY`: the window sees the rows before the request filtered the named
        // dimensions, so the fork keeps only the filters that do not mention them.
        let skip: Vec<String> = external
            .skip_filters
            .iter()
            .map(|sql| inline(sql, leaves))
            .collect();
        let fork_filters: Vec<String> = filters
            .iter()
            .filter(|sql| {
                !skip
                    .iter()
                    .any(|dimension| sql.contains(dimension.as_str()))
            })
            .cloned()
            .collect();
        let value_name = names.take();
        // `AVG([Delivered, %] FIXED [Region])` where the field is itself an aggregate formula
        // (`COUNTD(...) / COUNTD(...)`) cannot be one query: the argument aggregates, and the
        // LOD aggregate would wrap it (`avg(count(...) / count(...))`, SQLSTATE 42803). The fork
        // becomes two queries — the inner one computes the argument at the fork's grouping, the
        // outer one applies the call over the rows it produced — like the `INCLUDE` plan of a
        // measure above a fork already does. Only a LOD takes its level from the clause: a
        // lookup's and a window's fork have their own shape, and `TOTAL` is handled by
        // `nested_total_call` above.
        let nested_argument = if matches!(external.kind, dfl_native::ExternalKind::Lod(_))
            && aggregates_in_argument(external, leaves)
        {
            split_call(&external.sql).map(|(_, argument)| inline(argument, leaves))
        } else {
            None
        };
        // The request filters belong to the query that reads the source: in the two-level fork
        // that is the inner one, the outer query only sees the fork's columns.
        let (items, from, group_by, filters) = match nested_argument {
            Some(argument) => {
                let inner_alias = format!("f_{}", joins.len());
                let inner_value = names.take();
                let mut inner_items = dimension_items.clone();
                inner_items.push(SelectExpr {
                    sql: argument.clone(),
                    alias: inner_value.clone(),
                });
                let inner = Select {
                    items: inner_items,
                    from: base.from.clone(),
                    joins: Vec::new(),
                    filters: fork_filters.clone(),
                    group_by: fork_group_by.clone(),
                    having: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    ctes: Vec::new(),
                };
                let mut outer_items: Vec<SelectExpr> = dimension_items
                    .iter()
                    .map(|item| SelectExpr {
                        sql: format!("{inner_alias}.{}", item.alias),
                        alias: item.alias.clone(),
                    })
                    .collect();
                if let Some(key) = &constant_key {
                    outer_items.push(SelectExpr {
                        sql: "1".to_string(),
                        alias: key.clone(),
                    });
                }
                outer_items.push(SelectExpr {
                    sql: call.replacen(&argument, &format!("{inner_alias}.{inner_value}"), 1),
                    alias: value_name.clone(),
                });
                let group_by = dimension_items
                    .iter()
                    .map(|item| format!("{inner_alias}.{}", item.alias))
                    .collect();
                (
                    outer_items,
                    Source::Subquery {
                        select: Box::new(inner),
                        alias: inner_alias,
                    },
                    group_by,
                    Vec::new(),
                )
            }
            None => {
                let mut items = dimension_items.clone();
                if let Some(key) = &constant_key {
                    items.push(SelectExpr {
                        sql: "1".to_string(),
                        alias: key.clone(),
                    });
                }
                items.push(SelectExpr {
                    sql: call.clone(),
                    alias: value_name.clone(),
                });
                (
                    items,
                    base.from.clone(),
                    fork_group_by.clone(),
                    fork_filters.clone(),
                )
            }
        };
        fork_names[fork_index] = Some((alias.clone(), value_name.clone()));
        let select = Select {
            items,
            from,
            joins: Vec::new(),
            filters,
            group_by,
            having: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            ctes: Vec::new(),
        };
        fork_columns[fork_index] = Some(ForkColumns {
            select: select.clone(),
            value: value_name.clone(),
            keys,
        });
        if joined_to_base {
            joins.push(Join {
                source: Source::Subquery {
                    select: Box::new(select),
                    alias,
                },
                on,
                // A lookup keeps the rows with no match: the first period has no previous one.
                left: lookup.is_some(),
            });
        }
    }

    for (index, fork_index) in &dependent {
        let leaf = &leaves[*index];
        let Some(columns) = fork_columns[*fork_index].as_ref() else {
            continue;
        };
        let alias = format!("q_{}", joins.len() + 1);
        // A window over a fork cannot be computed from the fork's own query: its partition and
        // ordering are written over the *source* (`t0."date"`), and so is the aggregate its
        // argument is built from. This level therefore keeps the source and joins the fork to
        // it, grouped by the dimensions the fork was grouped by — one row per value the window
        // sees, which is exactly the grain the window partitions over.
        if leaf.window {
            let fork_alias = format!("f_{}", joins.len());
            // The window is written over the *source* (`PARTITION BY {leaf:2}`, the argument is
            // the fork's column), and the straightforward plan groups the source rows (`11 780`
            // on the cohort chart), sorts them and only then reduces to the `56` groups — the
            // window is computed over 11 780 rows instead of 56. Lifting the aggregation into an
            // inner query and the window into a query above it keeps the same values and lets
            // PostgreSQL run `Group(56) -> WindowAgg(56)`. If the window mentions anything the
            // two levels cannot express (another base column), the old single-level plan stays.
            let lifted = {
                // The window's partition/order are written over the source, but every one of
                // them is a dimension the fork already groups by, and the argument is the fork's
                // value. The fork is therefore the only input this level needs: `56` rows and a
                // window over them, instead of joining the `12 031` source rows to the fork and
                // reducing them again. When the window mentions anything else, the old plan stays.
                let mut dimensions: Vec<(String, String, String)> =
                    Vec::with_capacity(columns.keys.len());
                let mut outer_on: Vec<String> = Vec::with_capacity(columns.keys.len());
                for (dimension, base_name, fork_name) in &columns.keys {
                    let name = names.take();
                    dimensions.push((dimension.clone(), name.clone(), fork_name.clone()));
                    outer_on.push(nulls_equal("q_0", base_name, &alias, &name));
                }
                let mut sql = leaf.sql.clone();
                let mut ok = true;
                for (other, other_leaf) in leaves.iter().enumerate() {
                    if other_leaf.external.is_none() {
                        continue;
                    }
                    let placeholder = format!("{{leaf:{other}}}");
                    if !sql.contains(&placeholder) {
                        continue;
                    }
                    let Some(other_columns) = fork_columns[other].as_ref() else {
                        ok = false;
                        break;
                    };
                    sql = sql.replace(
                        &placeholder,
                        &format!("{fork_alias}.{}", other_columns.value),
                    );
                }
                if ok {
                    sql = inline(&sql, leaves);
                    for (dimension, _, fork_name) in &dimensions {
                        sql = sql.replace(dimension.as_str(), &format!("{fork_alias}.{fork_name}"));
                    }
                }
                if ok && !sql.contains("{leaf:") && !sql.contains("t0.") {
                    let value_name = names.take();
                    let mut outer_items: Vec<SelectExpr> = dimensions
                        .iter()
                        .map(|(_, name, fork_name)| SelectExpr {
                            sql: format!("{fork_alias}.{fork_name}"),
                            alias: name.clone(),
                        })
                        .collect();
                    outer_items.push(SelectExpr {
                        sql,
                        alias: value_name.clone(),
                    });
                    let outer = Select {
                        items: outer_items,
                        from: Source::Subquery {
                            select: Box::new(columns.select.clone()),
                            alias: fork_alias.clone(),
                        },
                        joins: Vec::new(),
                        filters: Vec::new(),
                        group_by: Vec::new(),
                        having: Vec::new(),
                        order_by: Vec::new(),
                        limit: None,
                        offset: None,
                        ctes: Vec::new(),
                    };
                    Some((outer, value_name, outer_on))
                } else {
                    None
                }
            };
            if let Some((outer, value_name, outer_on)) = lifted {
                fork_names[*index] = Some((alias.clone(), value_name));
                joins.push(Join {
                    source: Source::Subquery {
                        select: Box::new(outer),
                        alias: alias.clone(),
                    },
                    on: outer_on,
                    left: false,
                });
                continue;
            }
            let mut items: Vec<SelectExpr> = Vec::with_capacity(columns.keys.len() + 1);
            let mut group_by: Vec<String> = Vec::with_capacity(columns.keys.len() * 2);
            let mut on: Vec<String> = Vec::with_capacity(columns.keys.len());
            let mut outer_on: Vec<String> = Vec::with_capacity(columns.keys.len());
            for (dimension, base_name, fork_name) in &columns.keys {
                let name = names.take();
                items.push(SelectExpr {
                    sql: dimension.clone(),
                    alias: name.clone(),
                });
                group_by.push(dimension.clone());
                on.push(nulls_equal_expr(
                    dimension,
                    &format!("{fork_alias}.{fork_name}"),
                ));
                outer_on.push(nulls_equal("q_0", base_name, &alias, &name));
            }
            // The window itself, with the fork's column where its leaf was and the rest put back
            // as the SQL it is written in (`count(DISTINCT t0."user_id")`, the partition).
            let mut sql = leaf.sql.clone();
            for (other, other_leaf) in leaves.iter().enumerate() {
                if other_leaf.external.is_none() {
                    continue;
                }
                let replacement = match &fork_columns[other] {
                    Some(other_columns) => format!("{fork_alias}.{}", other_columns.value),
                    None => continue,
                };
                if !sql.contains(&format!("{{leaf:{other}}}")) {
                    continue;
                }
                sql = sql.replace(&format!("{{leaf:{other}}}"), &replacement);
                // A value the window is built from has to be a grouped column here: the level
                // groups the source rows, and the join does not tell it that the fork's column
                // follows from the dimensions. Grouping by it changes nothing else — the join
                // matches the fork on exactly those dimensions.
                group_by.push(replacement);
            }
            let value_name = names.take();
            items.push(SelectExpr {
                sql: inline(&sql, leaves),
                alias: value_name.clone(),
            });
            fork_names[*index] = Some((alias.clone(), value_name));
            joins.push(Join {
                source: Source::Subquery {
                    select: Box::new(Select {
                        items,
                        from: base.from.clone(),
                        joins: vec![Join {
                            source: Source::Subquery {
                                select: Box::new(columns.select.clone()),
                                alias: fork_alias.clone(),
                            },
                            on,
                            left: false,
                        }],
                        filters: filters.clone(),
                        group_by,
                        having: Vec::new(),
                        order_by: Vec::new(),
                        limit: None,
                        offset: None,
                        ctes: Vec::new(),
                    }),
                    alias: alias.clone(),
                },
                on: outer_on,
                left: false,
            });
            continue;
        }
        let source = format!("f_{}", joins.len());
        let mut items: Vec<SelectExpr> = Vec::with_capacity(columns.keys.len() + 1);
        let mut group_by: Vec<String> = Vec::with_capacity(columns.keys.len());
        let mut on: Vec<String> = Vec::with_capacity(columns.keys.len());
        for (_, base_name, fork_name) in &columns.keys {
            let key = format!("{source}.{fork_name}");
            let name = names.take();
            items.push(SelectExpr {
                sql: key.clone(),
                alias: name.clone(),
            });
            group_by.push(key);
            on.push(nulls_equal("q_0", base_name, &alias, &name));
        }
        // `AVG(SUM([Sales] INCLUDE [Region]))`: the aggregate is applied to the fork's values,
        // grouped by the dimensions of the request — every one of them is a column of the
        // fork, because a LOD only ever adds dimensions to the grouping.
        let mut sql = leaf.sql.clone();
        for (other, other_leaf) in leaves.iter().enumerate() {
            if other_leaf.external.is_none() {
                continue;
            }
            let replacement = match &fork_columns[other] {
                Some(other_columns) => format!("{source}.{}", other_columns.value),
                None => continue,
            };
            sql = sql.replace(&format!("{{leaf:{other}}}"), &replacement);
        }
        let value_name = names.take();
        items.push(SelectExpr {
            sql: inline(&sql, leaves),
            alias: value_name.clone(),
        });
        fork_names[*index] = Some((alias.clone(), value_name));
        joins.push(Join {
            source: Source::Subquery {
                select: Box::new(Select {
                    items,
                    from: Source::Subquery {
                        select: Box::new(columns.select.clone()),
                        alias: source,
                    },
                    joins: Vec::new(),
                    filters: Vec::new(),
                    group_by,
                    having: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    ctes: Vec::new(),
                }),
                alias,
            },
            on,
            left: false,
        });
    }

    // A window the request *filters* on is a column of the query above the joins: the condition
    // compares the column by its alias, and `WHERE` cannot see the alias of the `SELECT` that
    // computes it. A leaf placeholder can be placed there (`dense_rank() OVER (ORDER BY
    // q_1.e_5)`); an expression that is not one has to stay in the base query.
    // A window the request filters on but does not select still needs a column of the base
    // query: the outer query compares that column, and the base is where the rows are grouped.
    for (filter, leaf) in window_filters.iter().zip(&filtered_windows) {
        if leaf.is_some() || base.items.iter().any(|item| item.alias == filter.alias) {
            continue;
        }
        base_items.push(SelectExpr {
            // The window may be built on a fork's column (`dense_rank() OVER (ORDER BY …)`):
            // the placeholder is kept here and filled in below, like every other base item.
            sql: inline_base(&filter.expr),
            alias: filter.alias.clone(),
        });
    }
    // The forks exist now, so a base item may name the column of one: a window over a window is
    // computed in the base query from the value the fork selected.
    let base_items: Vec<SelectExpr> = base_items
        .into_iter()
        .map(|item| SelectExpr {
            sql: place_forks(&item.sql, &fork_names),
            alias: item.alias,
        })
        .collect();

    let mut items: Vec<SelectExpr> = base
        .items
        .iter()
        .map(|item| SelectExpr {
            sql: place(&item.sql, leaves, &base_names, &fork_names),
            alias: item.alias.clone(),
        })
        .collect();
    // The window the request filters on is computed here, above the joins, so that the wrapper
    // around the plan can compare it.
    for (filter, leaf) in window_filters.iter().zip(&filtered_windows) {
        if leaf.is_none() || items.iter().any(|item| item.alias == filter.alias) {
            continue;
        }
        items.push(SelectExpr {
            sql: place(&filter.expr, leaves, &base_names, &fork_names),
            alias: filter.alias.clone(),
        });
    }
    let shared_source = if dialect == Dialect::Postgres {
        match &base.from {
            Source::Subquery { .. } => Some(base.from.clone()),
            Source::Table(raw) if derived_table(raw).is_some() => Some(base.from.clone()),
            Source::Table(_) | Source::Cte { .. } => None,
        }
    } else {
        None
    };
    let mut plan = Select {
        items,
        from: Source::Subquery {
            select: Box::new(Select {
                items: base_items,
                // The base is a sub-query over raw rows: its filters and grouping are the
                // compiler's, with the leaves inlined back, and the request's `ORDER BY` /
                // `LIMIT` stay outside — they order the joined result, not the base.
                filters: filters.clone(),
                group_by: group_by.iter().map(|sql| inline(sql, leaves)).collect(),
                having: having.clone(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                ctes: Vec::new(),
                ..base.clone()
            }),
            alias: "q_0".to_string(),
        },
        joins,
        filters: Vec::new(),
        group_by: Vec::new(),
        having: Vec::new(),
        order_by: base.order_by.clone(),
        limit: base.limit,
        offset: base.offset,
        ctes: Vec::new(),
    };
    if let Some(source) = shared_source {
        // All forks read the same derived table. Hoisting it as a materialized CTE means the
        // database builds it once instead of once per fork; this is the query-plan equivalent of
        // factoring out an expression, not a hand-written query rewrite.
        let references = count_source(&plan, &source);
        if references >= 2 {
            let body_and_alias = match &source {
                Source::Subquery { select, alias } => Some((select.render(), alias.clone())),
                Source::Table(raw) => derived_table(raw),
                Source::Cte { .. } => None,
            };
            if let Some((body, alias)) = body_and_alias {
                let name = "dl_shared_source".to_string();
                plan.ctes.push(Cte {
                    name: name.clone(),
                    materialized: true,
                    body,
                });
                replace_source(&mut plan, &source, &Source::Cte { name, alias });
            }
        }
    }
    // A filter built on a window belongs to the query *above* it: `WHERE` cannot see the alias the
    // same `SELECT` gives the window's column, so the joined plan is wrapped once more and the
    // condition compares the wrapper's column. `QueryForkQuerySplitter` does the same.
    if window_filters.is_empty() {
        return Ok(plan);
    }
    Ok(filter_windows(plan, window_filters))
}

/// A raw `FROM` item that is really a parenthesised `SELECT ... AS alias`.
fn derived_table(raw: &str) -> Option<(String, String)> {
    let text = raw.trim();
    if !text.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '\'' {
                quote = false;
            }
            continue;
        }
        match ch {
            '\'' => quote = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let body = text[1..index].to_string();
                    if !body.trim_start().to_ascii_uppercase().starts_with("SELECT") {
                        return None;
                    }
                    let rest = text[index + 1..].trim();
                    let alias = rest
                        .strip_prefix("AS ")
                        .or_else(|| rest.strip_prefix("as "))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    return Some((body, alias));
                }
            }
            _ => {}
        }
    }
    None
}

fn count_source(select: &Select, target: &Source) -> usize {
    let mut count = usize::from(select.from == *target);
    if select.from != *target {
        if let Source::Subquery { select: inner, .. } = &select.from {
            count += count_source(inner, target);
        }
    }
    for join in &select.joins {
        if join.source == *target {
            count += 1;
        } else if let Source::Subquery { select: inner, .. } = &join.source {
            count += count_source(inner, target);
        }
    }
    count
}

fn replace_source(select: &mut Select, target: &Source, replacement: &Source) {
    if select.from == *target {
        select.from = replacement.clone();
    } else if let Source::Subquery { select: inner, .. } = &mut select.from {
        replace_source(inner, target, replacement);
    }
    for join in &mut select.joins {
        if join.source == *target {
            join.source = replacement.clone();
        } else if let Source::Subquery { select: inner, .. } = &mut join.source {
            replace_source(inner, target, replacement);
        }
    }
}

/// The conditions of the window filters, as the outermost query spells them.
///
/// The expression the condition was built from is replaced by the column the query below gives
/// it: `{leaf:7} <= 5` is `"371411f0-…" <= 5` once the window has an alias of its own.
fn window_conditions(filters: &[WindowFilter]) -> Vec<String> {
    filters
        .iter()
        .map(|filter| {
            filter
                .condition
                .replace(filter.expr.as_str(), &filter.alias)
        })
        .collect()
}

/// Give every window filter a column in the query below, when the request does not select it.
fn add_window_columns(mut select: Select, filters: &[WindowFilter], leaves: &[Leaf]) -> Select {
    for filter in filters {
        if select.items.iter().any(|item| item.alias == filter.alias) {
            continue;
        }
        select.items.push(SelectExpr {
            sql: inline(&filter.expr, leaves),
            alias: filter.alias.clone(),
        });
    }
    select
}

/// Put the query under a sub-query and apply the window filters to the query above it.
fn filter_windows(inner: Select, filters: &[WindowFilter]) -> Select {
    let items: Vec<SelectExpr> = inner
        .items
        .iter()
        .map(|item| SelectExpr {
            sql: item.alias.clone(),
            alias: item.alias.clone(),
        })
        .collect();
    // The `ORDER BY` and the page of the request belong to the outer query: they order and cut
    // the filtered rows, not the rows the windows were computed over.
    let (order_by, limit, offset) = (inner.order_by.clone(), inner.limit, inner.offset);
    Select {
        items,
        from: Source::Subquery {
            select: Box::new(Select {
                order_by: Vec::new(),
                limit: None,
                offset: None,
                ctes: Vec::new(),
                ..inner
            }),
            alias: "w_0".to_string(),
        },
        joins: Vec::new(),
        filters: window_conditions(filters),
        group_by: Vec::new(),
        having: Vec::new(),
        order_by,
        limit,
        offset,
        ctes: Vec::new(),
    }
}

/// Which leaves are a whole `SELECT` item on their own.
///
/// Only those become columns when they do not aggregate: a leaf that is nested inside another
/// one is inlined, in the base item and in the outer template alike.
fn top_level_leaves(select: &Select, leaves: &[Leaf]) -> Vec<bool> {
    let mut top = vec![false; leaves.len()];
    for item in &select.items {
        for (index, leaf) in leaves.iter().enumerate() {
            if item.sql == format!("{{leaf:{index}}}") && !leaf.external.is_some() {
                top[index] = true;
            }
        }
    }
    top
}

/// Which leaves the plan's templates mention.
fn used_leaves(select: &Select, leaves: &[Leaf]) -> Vec<bool> {
    let mut used = vec![false; leaves.len()];
    for item in &select.items {
        mark_leaves(&item.sql, &mut used);
    }
    for sql in select
        .filters
        .iter()
        .chain(&select.group_by)
        .chain(&select.having)
        .chain(&select.order_by)
    {
        mark_leaves(sql, &mut used);
    }
    // A leaf is often a template of its own — a window over an aggregate, `AVG({leaf:1}) OVER
    // (...)`, or a division of two aggregates. Everything such a leaf mentions has to be a
    // column of the base query as well, or the template would keep a `{leaf:N}` placeholder.
    // The loop runs to a fixed point: a leaf may mention an *earlier* leaf.
    loop {
        let before = used.clone();
        for (index, leaf) in leaves.iter().enumerate() {
            if used[index] {
                mark_leaves(&leaf.sql, &mut used);
            }
        }
        if used == before {
            break;
        }
    }
    used
}

/// Does the template mention leaf `index`?
fn mentions(template: &str, index: usize) -> bool {
    template.contains(&format!("{{leaf:{index}}}"))
}

/// The leaf a template is, when it is nothing but one placeholder: `{leaf:4}`, `({leaf:4})`.
fn single_leaf(template: &str) -> Option<usize> {
    // A field's SQL is wrapped in parentheses by the compiler, and the wrapper is not an
    // expression of its own: `({leaf:4})` is the same column as `{leaf:4}`.
    let text: String = template
        .chars()
        .filter(|letter| !matches!(letter, '(' | ')' | ' '))
        .collect();
    let index = text.strip_prefix("{leaf:")?.strip_suffix('}')?;
    if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// Mark every `{leaf:N}` of a template.
fn mark_leaves(template: &str, used: &mut [bool]) {
    let mut rest = template;
    while let Some(start) = rest.find("{leaf:") {
        let after = &rest[start + "{leaf:".len()..];
        let Some(end) = after.find('}') else { break };
        if let Ok(index) = after[..end].parse::<usize>() {
            if let Some(slot) = used.get_mut(index) {
                *slot = true;
            }
        }
        rest = &after[end..];
    }
}

/// `e_N` allocator, shared by the base and every fork.
struct Names {
    next: usize,
}

impl Names {
    fn take(&mut self) -> String {
        let name = format!("e_{}", self.next);
        self.next += 1;
        name
    }
}

/// `nulls_equal`: a LOD dimension may be `NULL`, and `NULL = NULL` is not true.
///
/// `dl_query_processing.multi_query.formula_compiler` spells the same `OR (... AND ... IS NULL)`
/// out instead of using `IS NOT DISTINCT FROM`, which not every dialect has.
fn nulls_equal(left_alias: &str, left: &str, right_alias: &str, right: &str) -> String {
    format!(
        "{left_alias}.{left} = {right_alias}.{right} \
         OR ({left_alias}.{left} IS NULL AND {right_alias}.{right} IS NULL)"
    )
}

/// The same for two expressions that are already spelled out: `AGO` joins on
/// `base.date = fork.date + INTERVAL '1 month'`, and the offset side is not a column name.
fn nulls_equal_expr(left: &str, right: &str) -> String {
    format!("{left} = {right} OR ({left} IS NULL AND {right} IS NULL)")
}

/// Replace `{leaf:N}` with the leaf's own SQL: the plan without forks is the compiler's SQL.
///
/// A leaf may itself be a template over other leaves (a window over an aggregate), and the
/// nested leaf may have a *lower* index than the outer one, so the replacement runs to a fixed
/// point instead of once per index.
fn inline(template: &str, leaves: &[Leaf]) -> String {
    let mut sql = template.to_string();
    loop {
        let mut replaced = false;
        for (index, leaf) in leaves.iter().enumerate() {
            let token = format!("{{leaf:{index}}}");
            // A leaf never mentions itself, but a malformed template would loop forever.
            if !sql.contains(&token) || leaf.sql.contains(&token) {
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

/// Replace the `{leaf:N}` of every fork with the column it selects: `q_1.e_3`.
///
/// Only a query that reads the fork as a source can do this — the base query, whose items are one
/// expression over the joined columns, and which is where a window over a fork is computed.
fn place_forks(template: &str, forks: &[Option<(String, String)>]) -> String {
    let mut sql = template.to_string();
    for (index, fork) in forks.iter().enumerate() {
        if let Some((alias, name)) = fork {
            sql = sql.replace(&format!("{{leaf:{index}}}"), &format!("{alias}.{name}"));
        }
    }
    sql
}

/// Replace `{leaf:N}` with the column of the sub-query that computes it.
///
/// A leaf that has no column (a nested, non-aggregated one — see [`top_level_leaves`]) is put
/// back as its own SQL, which may mention further leaves; the loop runs until none is left.
fn place(
    template: &str,
    leaves: &[Leaf],
    base: &[Option<String>],
    forks: &[Option<(String, String)>],
) -> String {
    let mut sql = template.to_string();
    loop {
        let mut replaced = false;
        for index in 0..base.len() {
            let token = format!("{{leaf:{index}}}");
            if !sql.contains(&token) {
                continue;
            }
            let replacement = match (&base[index], &forks[index]) {
                (Some(name), _) => format!("q_0.{name}"),
                (None, Some((alias, name))) => format!("{alias}.{name}"),
                // No column: the leaf is inlined, exactly as the base item inlines it.
                (None, None) => match leaves.get(index) {
                    Some(leaf) if !leaf.sql.contains(&token) => leaf.sql.clone(),
                    _ => continue,
                },
            };
            sql = sql.replace(&token, &replacement);
            replaced = true;
        }
        if !replaced {
            return sql;
        }
    }
}

/// `dl_formula` aggregates that cannot be computed from the raw rows when the level of
/// aggregation changes: `AVG([Sales] TOTAL)` averages the *grouped* sums, not the source values.
///
/// The other aggregates (`SUM`, `MIN`, `MAX`, `COUNT`) are distributive, so a fork over the source
/// returns the same number and the port keeps its single query for them.
pub(crate) fn is_distributive_aggregate(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "sum" | "sum_if" | "min" | "max" | "count" | "count_if" | "least" | "greatest"
    )
}

/// Whether the argument of the rendered call aggregates by itself.
///
/// `AVG([Delivered, %] FIXED [Region])`, where the field is a formula that already aggregates
/// (`COUNTD(...) / [Orders]`), renders as `avg(count(DISTINCT ...) / count(DISTINCT ...))`:
/// the LOD aggregate and the argument's own aggregate would sit in one query, which PostgreSQL
/// rejects (SQLSTATE 42803). The field's own aggregation is only dropped inside an aggregate
/// when the field declares one (`AVG([Sales] FIXED [d])` is `avg(t0."amount")`), so the check
/// looks at the leaves the argument is built from: a plain column leaf is not aggregated, an
/// aggregate call leaf is.
fn aggregates_in_argument(external: &ExternalCall, leaves: &[Leaf]) -> bool {
    let Some((_, argument)) = split_call(&external.sql) else {
        return false;
    };
    leaves
        .iter()
        .enumerate()
        .any(|(index, leaf)| leaf.aggregated && mentions(argument, index))
}

/// `fn(arg)` -> `("fn", "arg")`, for the rendered call of a fork.
fn split_call(call: &str) -> Option<(&str, &str)> {
    let open = call.find('(')?;
    if !call.trim_end().ends_with(')') {
        return None;
    }
    let name = call[..open].trim();
    let argument = &call[open + 1..call.trim_end().len() - 1];
    (!name.is_empty() && !argument.is_empty()).then_some((name, argument))
}

/// A `TOTAL` aggregate that is not distributive: it has to be computed over the rows the base
/// query groups, not over the rows of the source.
///
/// Returns `(function name, argument)` when the fork has to become two queries.
fn nested_total_call<'a>(
    external: &ExternalCall,
    call: &'a str,
    globals: &[String],
) -> Option<(&'a str, &'a str)> {
    if !matches!(external.kind, dfl_native::ExternalKind::Total) || globals.is_empty() {
        return None;
    }
    let (name, argument) = split_call(call)?;
    (!is_distributive_aggregate(name)).then_some((name, argument))
}
