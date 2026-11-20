//! The query plan — `dl_query_processing.compilation.primitives`' `CompiledQuery`, cut down to
//! what the port needs.
//!
//! The compiler used to assemble the `SELECT` string as it walked the request, which left no
//! place for anything a single `SELECT` cannot express: an aggregation by a level of detail
//! (`COUNTD([user] FIXED [dim])`), a window function, a `TOTAL`. `dl_query_processing` builds
//! those as *several* queries joined together (`QueryFork` -> `q_0`, `q_1`, ...), and this module
//! is where those queries live now.
//!
//! The plan is a tree of [`Select`]s: a node reads either from a table or from another node, and
//! may join more sources to it. Rendering is a plain recursive walk, so the SQL of a plan with no
//! sub-query is exactly what [`crate::compile`] produced before the plan existed.

/// One `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    pub items: Vec<SelectExpr>,
    pub from: Source,
    pub joins: Vec<Join>,
    pub filters: Vec<String>,
    pub group_by: Vec<String>,
    pub having: Vec<String>,
    pub order_by: Vec<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// CTEs defined by this select, e.g. a repeated source hoisted by the fork pass.
    pub ctes: Vec<Cte>,
}

/// A hoisted common subquery: `WITH <name> AS MATERIALIZED (<select>) ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cte {
    pub name: String,
    pub materialized: bool,
    /// The rendered body; a hoisted `Source::Subquery` is rendered when the CTE is built, and a
    /// derived table written as raw SQL keeps its text.
    pub body: String,
}

/// A `SELECT` item: the expression and the name of the column it produces.
///
/// The alias is already quoted — `dl_api_lib` names result columns by field `guid`, and the
/// renderer must not learn about datasets to keep writing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectExpr {
    pub sql: String,
    pub alias: String,
}

/// What a [`Select`] reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A rendered `FROM` clause over the dataset's tables, e.g. `orders AS t0`.
    Table(String),
    /// Another node of the plan, rendered as `(<select>) <alias>`.
    Subquery { select: Box<Select>, alias: String },
    /// A reference to a CTE hoisted by [`Select::ctes`], keeping the original alias.
    Cte { name: String, alias: String },
}

/// A joined source: `JOIN (<select>) q_1 ON q_0.e_4 = q_1.e_2 OR (...)`.
///
/// The conditions are joined with `AND`, each in parentheses: the `OR` of a fork's join stays
/// inside one condition (`dl_query_processing.multi_query.formula_compiler` renders
/// `nulls_equal` that way) and the `AND` must not be absorbed by it.
///
/// An **empty** `on` is a join by a constant key and renders as `CROSS JOIN`: the fork of a
/// `TOTAL` or of an ungrouped `FIXED` is exactly one row, so there is no column to match on
/// (see [`crate::fork`]). Spelling it `CROSS JOIN` instead of Python's
/// `ON (q_0.e_5 = q_1.e_6 OR (… IS NULL …))` is what keeps the fork from being re-executed once
/// per row of the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    pub source: Source,
    pub on: Vec<String>,
    /// `LEFT JOIN`: a lookup (`AGO`) keeps the rows that have no match, the previous period
    /// of the first one does not exist. A `QueryFork` joins with `JoinType.inner`.
    pub left: bool,
}

impl Select {
    fn render_source(source: &Source) -> String {
        match source {
            Source::Table(sql) => sql.clone(),
            Source::Subquery { select, alias } => {
                format!("({}) {}", select.render(), alias)
            }
            Source::Cte { name, alias } if alias.is_empty() => name.clone(),
            Source::Cte { name, alias } => format!("{name} AS {alias}"),
        }
    }

    /// The plan's SQL, in `dl_query_processing.translation`'s clause order.
    pub fn render(&self) -> String {
        let mut prefix = String::new();
        if !self.ctes.is_empty() {
            prefix.push_str("WITH ");
            for (index, cte) in self.ctes.iter().enumerate() {
                if index > 0 {
                    prefix.push_str(", ");
                }
                prefix.push_str(&cte.name);
                if cte.materialized {
                    prefix.push_str(" AS MATERIALIZED ");
                } else {
                    prefix.push_str(" AS ");
                }
                prefix.push('(');
                prefix.push_str(&cte.body);
                prefix.push(')');
            }
            prefix.push(' ');
        }
        let mut sql = format!(
            "{}SELECT {} FROM {}",
            prefix,
            self.items
                .iter()
                .map(|item| format!("{} AS {}", item.sql, item.alias))
                .collect::<Vec<_>>()
                .join(", "),
            Self::render_source(&self.from),
        );
        for join in &self.joins {
            let source = Self::render_source(&join.source);
            // A join without a condition is a join by a constant key: the `TOTAL` / ungrouped
            // `FIXED` fork is exactly one row, so the relation is a `CROSS JOIN`. It matters
            // that it is spelled that way: PostgreSQL cannot prove Python's `ON (1 = 1)`
            // tautology and re-runs the fork for every row of the base (`Nested Loop
            // loops=8146`, 8-10 s on the drill-down table), while a `CROSS JOIN` executes the
            // one-row side once.
            if join.on.is_empty() && !join.left {
                sql.push_str(" CROSS JOIN ");
                sql.push_str(&source);
                continue;
            }
            sql.push_str(if join.left { " LEFT JOIN " } else { " JOIN " });
            sql.push_str(&source);
            if !join.on.is_empty() {
                sql.push_str(" ON ");
                // Every condition keeps its parentheses: `nulls_equal` ends in a top-level
                // `OR`, and `a = b OR (a IS NULL AND b IS NULL) AND c = d` is not the join
                // Python emits — it spends the `AND` on the fallback instead of the next key.
                sql.push_str(
                    &join
                        .on
                        .iter()
                        .map(|condition| format!("({condition})"))
                        .collect::<Vec<_>>()
                        .join(" AND "),
                );
            }
        }
        if !self.filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.filters.join(" AND "));
        }
        if !self.group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&self.group_by.join(", "));
        }
        if !self.having.is_empty() {
            sql.push_str(" HAVING ");
            sql.push_str(&self.having.join(" AND "));
        }
        if !self.order_by.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(&self.order_by.join(", "));
        }
        if let Some(limit) = self.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = self.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
        sql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(sql: &str, alias: &str) -> SelectExpr {
        SelectExpr {
            sql: sql.to_string(),
            alias: alias.to_string(),
        }
    }

    fn from_table(sql: &str) -> Select {
        Select {
            items: vec![],
            from: Source::Table(sql.to_string()),
            joins: vec![],
            filters: vec![],
            group_by: vec![],
            having: vec![],
            order_by: vec![],
            limit: None,
            offset: None,
            ctes: Vec::new(),
        }
    }

    #[test]
    fn a_plan_without_subqueries_renders_as_one_select() {
        let plan = Select {
            items: vec![
                column("t0.\"category\"", "\"category\""),
                column("count(t0.\"id\")", "\"cnt\""),
            ],
            filters: vec!["t0.\"region\" = 'eu'".to_string()],
            group_by: vec!["t0.\"category\"".to_string()],
            having: vec!["count(t0.\"id\") > 1".to_string()],
            order_by: vec!["2 DESC".to_string()],
            limit: Some(10),
            offset: Some(5),
            ctes: Vec::new(),
            ..from_table("orders AS t0")
        };
        assert_eq!(
            plan.render(),
            "SELECT t0.\"category\" AS \"category\", count(t0.\"id\") AS \"cnt\" \
             FROM orders AS t0 WHERE t0.\"region\" = 'eu' GROUP BY t0.\"category\" \
             HAVING count(t0.\"id\") > 1 ORDER BY 2 DESC LIMIT 10 OFFSET 5"
        );
    }

    #[test]
    fn a_materialized_cte_renders_before_the_select() {
        let source = from_table("orders AS t0");
        let plan = Select {
            ctes: vec![Cte {
                name: "dl_shared_source".to_string(),
                materialized: true,
                body: source.render(),
            }],
            items: vec![column("t0.\"id\"", "\"id\"")],
            from: Source::Cte {
                name: "dl_shared_source".to_string(),
                alias: "t0".to_string(),
            },
            ..from_table("unused")
        };
        assert_eq!(
            plan.render(),
            "WITH dl_shared_source AS MATERIALIZED (SELECT  FROM orders AS t0) SELECT t0.\"id\" AS \"id\" FROM dl_shared_source AS t0"
        );
    }

    #[test]
    fn a_fork_joins_its_subquery_to_the_base() {
        // The shape `QueryForkQuerySplitter` builds for `COUNTD([user] FIXED [dim])`: the fork
        // is its own `GROUP BY`, and the base joins it on the fork's dimensions, with
        // `nulls_equal` for the key.
        let mut fork = from_table("orders AS t1");
        fork.items = vec![
            column("t1.\"dim\"", "e_0"),
            column("count(DISTINCT t1.\"user\")", "e_1"),
        ];
        fork.group_by = vec!["t1.\"dim\"".to_string()];

        let mut constant = from_table("orders AS t2");
        constant.items = vec![column("1", "e_2")];

        let base = Select {
            items: vec![column("q_0.e_1", "\"cnt\"")],
            from: Source::Subquery {
                select: Box::new(fork),
                alias: "q_0".to_string(),
            },
            joins: vec![Join {
                source: Source::Subquery {
                    select: Box::new(constant),
                    alias: "q_1".to_string(),
                },
                on: vec!["q_0.e_0 = q_1.e_2 OR (q_0.e_0 IS NULL AND q_1.e_2 IS NULL)".to_string()],
                left: false,
            }],
            ..from_table("unused")
        };
        assert_eq!(
            base.render(),
            "SELECT q_0.e_1 AS \"cnt\" \
             FROM (SELECT t1.\"dim\" AS e_0, count(DISTINCT t1.\"user\") AS e_1 \
                   FROM orders AS t1 GROUP BY t1.\"dim\") q_0 \
             JOIN (SELECT 1 AS e_2 FROM orders AS t2) q_1 \
             ON (q_0.e_0 = q_1.e_2 OR (q_0.e_0 IS NULL AND q_1.e_2 IS NULL))"
        );
    }

    #[test]
    fn a_join_without_conditions_renders_as_a_cross_join() {
        // A `TOTAL` / ungrouped `FIXED` fork returns exactly one row, so there is no key to
        // match: Python still writes the constant key into both queries and joins with
        // `nulls_equal`, which PostgreSQL cannot fold into a tautology. It then runs the fork
        // once per row of the base — the drill-down table's `Nested Loop ... loops=8146`,
        // 8-10 s — while the same relation spelled `CROSS JOIN` runs it once (39 ms).
        let mut fork = from_table("orders AS t1");
        fork.items = vec![
            column("1", "e_2"),
            column("avg(f_0.e_4)", "e_3"),
        ];
        let base = Select {
            items: vec![column("q_0.e_1", "\"sum\""), column("q_1.e_3", "\"avg\"")],
            from: Source::Subquery {
                select: Box::new(from_table("orders AS t0")),
                alias: "q_0".to_string(),
            },
            joins: vec![Join {
                source: Source::Subquery {
                    select: Box::new(fork),
                    alias: "q_1".to_string(),
                },
                on: Vec::new(),
                left: false,
            }],
            ..from_table("unused")
        };
        assert_eq!(
            base.render(),
            "SELECT q_0.e_1 AS \"sum\", q_1.e_3 AS \"avg\" \
             FROM (SELECT  FROM orders AS t0) q_0 \
             CROSS JOIN (SELECT 1 AS e_2, avg(f_0.e_4) AS e_3 FROM orders AS t1) q_1"
        );
    }
}
