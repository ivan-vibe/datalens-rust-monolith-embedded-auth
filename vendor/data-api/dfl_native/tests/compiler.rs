//! Tests of the compiler itself (no Python involved).

use dfl_native::{
    compile_formula, compile_formula_named, parse_formula, DataType, FieldName, FieldNames,
    FieldTypes,
};

fn fields() -> FieldTypes {
    [
        ("i", DataType::Integer),
        ("f", DataType::Float),
        ("s", DataType::String),
        ("d", DataType::Date),
        ("dt", DataType::Datetime),
        ("dtg", DataType::GenericDatetime),
        ("b", DataType::Boolean),
    ]
    .iter()
    .map(|(name, dtype)| (name.to_string(), *dtype))
    .collect()
}

fn sql(formula: &str, dialect: &str) -> String {
    compile_formula(formula, dialect, &fields()).unwrap().sql
}

#[test]
fn parse_errors_keep_the_fields_dl_formula_reports() {
    // `(#code, message, token, start, end)` — exactly what `FormulaErrorCtx` serialises.
    fn error(formula: &str) -> (String, String, Option<String>, Option<u32>, Option<u32>) {
        let error = dfl_native::parse_formula_typed(formula).unwrap_err();
        assert!(error.exact, "{formula}: {error:?} must claim exactness");
        let (start, end) = match error.position {
            Some(span) => (Some(span.start), Some(span.end)),
            None => (None, None),
        };
        (
            error.code.to_string(),
            error.message,
            error.token,
            start,
            end,
        )
    }

    // A token the lexer cannot build is reported as the single character it starts with.
    assert_eq!(
        error("[a"),
        (
            "FORMULA.PARSE.UNEXPECTED_TOKEN".to_string(),
            "Failed to parse formula: unexpected token".to_string(),
            Some("[".to_string()),
            Some(0),
            Some(0),
        )
    );
    assert_eq!(error("'abc").2.as_deref(), Some("'"));
    // The grammar has no unary plus.
    assert_eq!(error("+ 2").2.as_deref(), Some("+"));
    // A multi-word token is reported as one token, with the spaces it was written with.
    assert_eq!(error("[a] ORDER BY").2.as_deref(), Some("ORDER BY"));
    // A clause outside the argument list is a syntax error on the clause keyword.
    assert_eq!(error("SUM([a]) WITHIN [b]").2.as_deref(), Some("WITHIN"));
    // Nothing but comments is not an empty formula: ANTLR sees an empty token stream.
    assert_eq!(
        error("-- no formula here"),
        (
            "FORMULA.PARSE.UNEXPECTED_EOF.EMPTY_FORMULA".to_string(),
            "Empty formula is invalid".to_string(),
            None,
            None,
            None,
        )
    );
    assert_eq!(error("").1, "Empty formula");
}

#[test]
fn date_literals_are_checked_like_dl_formula() {
    // A valid literal parses: `Item::Date` keeps the text between the hashes.
    for formula in [
        "#2020-01-01#",
        "##2020-01-01##",
        "#2020-01-01 10:00:00#",
        "#2019-01-01T#",
        "# 2020-01-01#",
    ] {
        assert!(parse_formula(formula).is_ok(), "{formula} must parse");
    }

    // An impossible date is a `ParseDateValueError`/`ParseDatetimeValueError`, with the whole
    // literal as the position and its text as the token.
    let error = dfl_native::parse_formula_typed("#2020-13-01#").unwrap_err();
    assert_eq!(error.code, "FORMULA.PARSE.VALUE.DATE");
    assert_eq!(error.message, "Invalid date value: 2020-13-01");
    assert_eq!(error.token.as_deref(), Some("2020-13-01"));
    assert_eq!(
        (error.position.unwrap().start, error.position.unwrap().end),
        (0, 12)
    );

    let error = dfl_native::parse_formula_typed("#2020-01-01 25#").unwrap_err();
    assert_eq!(error.code, "FORMULA.PARSE.VALUE.DATETIME");
    assert_eq!(error.message, "Invalid datetime value: 2020-01-01 25");
    assert_eq!(
        (error.position.unwrap().start, error.position.unwrap().end),
        (0, 15)
    );

    // The hashes and the inner text are separate tokens, so the broken pair is reported as the
    // token that does not fit — exactly like ANTLR.
    let error = dfl_native::parse_formula_typed("#2020-01-01X#").unwrap_err();
    assert_eq!(error.token.as_deref(), Some("X"));
    assert_eq!(
        (error.position.unwrap().start, error.position.unwrap().end),
        (11, 11)
    );
    let error = dfl_native::parse_formula_typed("#2020-01-01##").unwrap_err();
    assert_eq!(error.token.as_deref(), Some("##"));

    // A date literal is a typed literal per dialect, the same spelling `dl_query` uses for a
    // date filter value: a `CAST` on PostgreSQL/MySQL, `toDate32`/`toDateTime` on ClickHouse.
    // (`dl_formula` emits a bind parameter there; this engine builds one statement.)
    assert_eq!(
        sql("#2020-01-01#", "postgres"),
        "CAST('2020-01-01' AS DATE)"
    );
    assert_eq!(sql("#2020-01-01#", "mysql"), "CAST('2020-01-01' AS DATE)");
    assert_eq!(sql("#2020-01-01#", "clickhouse"), "toDate32('2020-01-01')");
    assert_eq!(
        sql("##2020-01-01 10:00:00##", "postgres"),
        "CAST('2020-01-01 10:00:00' AS TIMESTAMP)"
    );
    assert_eq!(
        sql("##2020-01-01 10:00:00##", "clickhouse"),
        "toDateTime('2020-01-01 10:00:00')"
    );
}

#[test]
fn non_breaking_spaces_are_whitespace_like_the_grammar() {
    // `WS: [ \t\n\r\f\u00a0]+ -> skip`. The demo's currency formula is written with
    // non-breaking spaces, and a byte-wise scan must not read the leading `0xc2` as `Â`.
    assert_eq!(sql("[i]\u{a0}+\u{a0}[i]", "postgres"), "i + i");
    assert_eq!(
        sql("IF(\u{a0}[b],\u{a0}1,\u{a0}0)", "postgres"),
        "CASE WHEN b THEN 1 ELSE 0 END"
    );
    // A string literal keeps its non-breaking space: the grammar only skips `WS` between tokens.
    assert_eq!(sql("'a\u{a0}b'", "postgres"), "'a\u{a0}b'");
}

#[test]
fn arithmetic_is_dialect_independent() {
    for dialect in ["postgres", "mysql", "clickhouse"] {
        assert_eq!(sql("[i] + [i] * [i]", dialect), "i + i * i");
        assert_eq!(sql("([i] + [i]) * [i]", dialect), "(i + i) * i");
        assert_eq!(sql("[i] - ([i] - [i])", dialect), "i - (i - i)");
    }
}

#[test]
fn string_concatenation_depends_on_argument_types() {
    assert_eq!(sql("[s] + [s]", "postgres"), "s || s");
    assert_eq!(sql("[s] + [s]", "mysql"), "concat(s, s)");
    assert_eq!(sql("[s] + 'x'", "postgres"), "s || 'x'");
}

#[test]
fn constant_arguments_are_part_of_the_lookup_key() {
    assert_eq!(
        sql("DATEADD('day', 7, [d])", "postgres"),
        "CAST(d + INTERVAL '7 DAY' AS DATE)"
    );
    assert_eq!(sql("DATEADD('day', 7, [d])", "mysql"), "d + INTERVAL 7 DAY");
    assert_eq!(
        sql("DATEADD('day', 7, [d])", "clickhouse"),
        "CAST(d + toIntervalDay(ifNull(7, 0)) AS Nullable(Date))"
    );
    // both the documented and the "legacy" argument orders are supported
    assert_eq!(
        sql("DATEADD([d], 'day', 7)", "postgres"),
        "CAST(d + INTERVAL '7 DAY' AS DATE)"
    );
    assert_eq!(
        sql("DATEPART('year', [d])", "postgres"),
        "CAST(DATE_PART('year', d) AS BIGINT)"
    );
}

#[test]
fn percent_in_string_literals_is_not_escaped() {
    // `dl_formula` doubles `%` for SQLAlchemy's paramstyle and SQLAlchemy undoes it, so the
    // database sees one sign. The port writes the statement itself: `'%'` stays `'%'`.
    assert_eq!(
        sql("STR([i]) + '%'", "postgres"),
        "TO_CHAR(i, 'FMMI99999999999999999999') || '%'"
    );
}

#[test]
fn percent_in_a_template_is_not_escaped() {
    // The registry keeps the templates as SQLAlchemy renders them, where the DBAPI's `%`
    // paramstyle doubles every `%`: the modulo operator and a `LIKE` pattern would otherwise
    // reach the server as `%%`.
    assert_eq!(sql("[i] % 2", "postgres"), "i % 2");
    assert_eq!(sql("[f] % 2.5", "mysql"), "f % 2.5");
    assert_eq!(
        sql("CONTAINS([s], 'week')", "clickhouse"),
        "s LIKE '%week%'"
    );
}

#[test]
fn case_and_if_render_per_dialect() {
    assert_eq!(
        sql("CASE [s] WHEN 'a' THEN 1 ELSE 0 END", "postgres"),
        "CASE s WHEN 'a' THEN 1 ELSE 0 END"
    );
    assert_eq!(
        sql("IF [b] THEN 1 ELSE 2 END", "postgres"),
        "CASE WHEN b THEN 1 ELSE 2 END"
    );
    assert_eq!(sql("IF [b] THEN 1 ELSE 2 END", "mysql"), "IF(b, 1, 2)");
    // `IF(...)`/`IIF(...)`/`CASE(...)` are the function spellings of the same blocks.
    assert_eq!(
        sql("IF([b], 1, 2)", "postgres"),
        "CASE WHEN b THEN 1 ELSE 2 END"
    );
    assert_eq!(
        sql("IIF([b], 1, 2)", "postgres"),
        "CASE WHEN b THEN 1 ELSE 2 END"
    );
    // The condition of a block may be parenthesized: it is still a block.
    assert_eq!(
        sql("IF ([b]) THEN 1 ELSE 2 END", "postgres"),
        "CASE WHEN b THEN 1 ELSE 2 END"
    );
    assert_eq!(
        sql("CASE([s], 'a', 1, 'b', 2, 0)", "postgres"),
        "CASE s WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 0 END"
    );
    // a multi-branch IF is a CASE in every dialect, and conditions get parentheses
    assert_eq!(
        sql("IF [b] THEN 1 ELSEIF [i] > 2 THEN 2 ELSE 3 END", "mysql"),
        "CASE WHEN b THEN 1 WHEN (i > 2) THEN 2 ELSE 3 END"
    );
}

#[test]
fn window_functions_use_within_as_partition() {
    assert_eq!(
        sql("SUM([f] WITHIN [s])", "postgres"),
        "sum(f) OVER (PARTITION BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"
    );
    assert!(
        sql("RANK([f] WITHIN [s])", "postgres").contains("rank() OVER (PARTITION BY s"),
        "unexpected window SQL"
    );
}

#[test]
fn window_frames_and_datetime_arithmetic_use_the_generalized_rows() {
    // Two numeric arguments: the registry used to hold only the frame sizes it had probed
    // (`1`, `2`, `3`), so `MAVG([Sales], 14, 14)` from the demo dashboard was unsupported.
    assert_eq!(
        sql("MAVG([f], 14, 14)", "postgres"),
        "AVG(f) OVER (ROWS BETWEEN 14 PRECEDING AND 14 FOLLOWING)"
    );
    assert_eq!(
        sql("MSUM([f], 3, 7)", "postgres"),
        "sum(f) OVER (ROWS BETWEEN 3 PRECEDING AND 7 FOLLOWING)"
    );
    // ... and the same values still match the probed rows they came from.
    assert_eq!(
        sql("MAVG([f], 2)", "postgres"),
        "AVG(f) OVER (ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)"
    );

    // `dl_formula` has no `(GENERICDATETIME, INTEGER)` translation for `-`; it resolves the
    // number through `CONST_INTEGER.casts_to(FLOAT)` and renders the `FLOAT` one:
    // `date_trunc('month', [d]) + INTERVAL '1 MONTH' - 1` is a real dashboard formula.
    assert_eq!(
        sql("[dtg] - 1", "postgres"),
        "dtg - CAST(CAST(1 AS TEXT) || ' day' AS INTERVAL (6))"
    );
    assert_eq!(
        sql("[dtg] + 1", "postgres"),
        "dtg + CAST(CAST(1 AS TEXT) || ' day' AS INTERVAL (6))"
    );
}

#[test]
fn clauses_other_than_a_plain_within_are_not_translated() {
    // The parser understands them (that is what makes the native analysis complete), but the
    // SQL of these formulas is produced by `dl_query_processing`, not by the function template:
    // rendering the bare call here would silently drop the level of detail or the filter.
    for formula in [
        "SUM([f] FIXED [s])",
        "SUM([f] INCLUDE [s])",
        "SUM([f] EXCLUDE [s])",
        "SUM([f] BEFORE FILTER BY [s])",
        "SUM([f] TOTAL)",
        "AGO([f], [d], 1 IGNORE DIMENSIONS [s])",
    ] {
        let err = compile_formula(formula, "postgres", &fields()).unwrap_err();
        assert!(
            err.contains("is not translated natively"),
            "{formula}: unexpected error {err}"
        );
    }
    // ... while the plain `WITHIN` partition keeps working
    assert!(compile_formula("SUM([f] WITHIN [s])", "postgres", &fields()).is_ok());
}

#[test]
fn among_is_relative_to_the_request() {
    // `AMONG [d]` is `WITHIN <the request's dimensions except d>`, so it needs the request: on
    // its own there is nothing to subtract the dimension from.
    let err = compile_formula("SUM([f] AMONG [s])", "postgres", &fields()).unwrap_err();
    assert!(
        err.contains("is not a dimension of the request"),
        "unexpected error {err}"
    );
    let window = dfl_native::WindowContext {
        group_dims: vec!["s".to_string(), "i".to_string()],
        ordering: vec![("d".to_string(), false)],
    };
    let compiled = dfl_native::compile_leaves_from(
        &parse_formula("SUM([f] AMONG [s])").unwrap(),
        dfl_native::Dialect::parse("postgres").unwrap(),
        &fields(),
        &FieldNames::default(),
        0,
        &window,
    )
    .unwrap();
    assert_eq!(compiled.sql, "{leaf:0}");
    assert_eq!(
        compiled.leaves[0].sql,
        // `SUM`'s window has no ordering slot: only the functions that order by default
        // (`RSUM`, `MAVG`, `RANK`) take one.
        "sum(f) OVER (PARTITION BY i ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"
    );
}

#[test]
fn a_bare_window_takes_the_request_ordering() {
    // `DefaultWindowOrderingMutation`: `RSUM([f])` without a `WITHIN` has no partition and
    // orders by the request's `ORDER BY`, which is what the engine fills `{O}` with.
    let window = dfl_native::WindowContext {
        group_dims: Vec::new(),
        ordering: vec![("d".to_string(), false)],
    };
    let compiled = dfl_native::compile_leaves_from(
        &parse_formula("RSUM([f])").unwrap(),
        dfl_native::Dialect::parse("postgres").unwrap(),
        &fields(),
        &FieldNames::default(),
        0,
        &window,
    )
    .unwrap();
    assert_eq!(compiled.sql, "{leaf:0}");
    assert_eq!(
        compiled.leaves[0].sql,
        "sum(f) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
    );
}

#[test]
fn a_window_over_a_window_moves_the_argument_down() {
    // `WinFuncQuerySplitter`: a window function is the topmost node of its query, so
    // `RSUM(SUM([i] WITHIN [s]))` is `sum(sum(i) OVER (...)) OVER (...)` — a window inside a
    // window, which Postgres rejects (`window function calls cannot be nested`, SQLSTATE 42P20).
    // The argument becomes a fork of its own and the outer window reads its column.
    let window = dfl_native::WindowContext {
        group_dims: vec!["s".to_string()],
        ordering: vec![("d".to_string(), false)],
    };
    let compiled = dfl_native::compile_leaves_from(
        &parse_formula("RSUM(SUM([i] WITHIN [s]))").unwrap(),
        dfl_native::Dialect::parse("postgres").unwrap(),
        &fields(),
        &FieldNames::default(),
        0,
        &window,
    )
    .unwrap();
    // The formula is the outer window, and its argument is a leaf of the plan.
    assert_eq!(compiled.sql, "{leaf:1}");
    assert_eq!(
        compiled.leaves[1].sql,
        "sum({leaf:0}) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
    );
    // The argument is the inner window, and it is a fork grouped by the request's dimensions.
    assert_eq!(
        compiled.leaves[0].sql,
        "sum(i) OVER (PARTITION BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"
    );
    let external = compiled.leaves[0]
        .external
        .as_ref()
        .expect("the inner window is forked");
    assert_eq!(external.kind, dfl_native::ExternalKind::Window);
    assert_eq!(external.dimensions, vec!["s".to_string()]);
}

#[test]
fn a_window_over_a_dimension_stays_one_query() {
    // `ROUND` is not a window: `ROUND(RSUM([f]))` is one `SELECT`, because the window is inside
    // the round and not inside another window. Nothing may be forked here.
    let window = dfl_native::WindowContext {
        group_dims: Vec::new(),
        ordering: vec![("d".to_string(), false)],
    };
    let compiled = dfl_native::compile_leaves_from(
        &parse_formula("ROUND(RSUM([f]))").unwrap(),
        dfl_native::Dialect::parse("postgres").unwrap(),
        &fields(),
        &FieldNames::default(),
        0,
        &window,
    )
    .unwrap();
    assert!(
        compiled.leaves.iter().all(|leaf| leaf.external.is_none()),
        "{:?}",
        compiled.leaves
    );
}

#[test]
fn between_and_like_are_supported() {
    assert_eq!(sql("[i] BETWEEN 1 AND 3", "postgres"), "i BETWEEN 1 AND 3");
    assert_eq!(
        sql("[i] NOT BETWEEN 1 AND 3", "postgres"),
        "i NOT BETWEEN 1 AND 3"
    );
    assert_eq!(sql("[s] LIKE 'a%'", "postgres"), "s LIKE 'a%'");
    assert_eq!(sql("[s] NOT LIKE 'a%'", "postgres"), "s NOT LIKE 'a%'");
}

#[test]
fn is_null_and_is_true_render_per_dialect() {
    // `IS NULL` is a comparison in PostgreSQL and an `ISNULL()`/`isNull()` call elsewhere.
    assert_eq!(sql("[i] IS NULL", "postgres"), "i IS NULL");
    assert_eq!(sql("[i] IS NULL", "mysql"), "ISNULL(i)");
    assert_eq!(sql("[i] IS NULL", "clickhouse"), "isNull(i)");
    // The negated form is a separate template: PostgreSQL keeps `IS NOT NULL`, the others
    // negate the `ISNULL()` call.
    assert_eq!(sql("[i] IS NOT NULL", "postgres"), "i IS NOT NULL");
    assert_eq!(sql("[i] IS NOT NULL", "mysql"), "NOT ISNULL(i)");
    assert_eq!(sql("[i] IS NOT NULL", "clickhouse"), "NOT isNull(i)");
    // The operand is parenthesized like a comparison.
    assert_eq!(sql("([i] = 1) IS NULL", "postgres"), "(i = 1) IS NULL");
    assert_eq!(sql("([i] = 1) IS NULL", "mysql"), "ISNULL(i = 1)");

    // `IS TRUE`/`IS FALSE` are comparisons against the type's zero value; booleans keep
    // the literal, and ClickHouse has no boolean type at all.
    assert_eq!(sql("[b] IS TRUE", "postgres"), "b IS true");
    assert_eq!(sql("[b] IS NOT FALSE", "postgres"), "b IS NOT false");
    assert_eq!(sql("[b] IS TRUE", "clickhouse"), "b != 0");
    assert_eq!(sql("[i] IS TRUE", "postgres"), "i != 0.0");
    assert_eq!(sql("[f] IS FALSE", "postgres"), "f = 0.0");
    assert_eq!(sql("[s] IS TRUE", "postgres"), "s != ''");
}

#[test]
fn in_lists_are_expanded_with_the_null_fix_up() {
    // A plain list becomes `IN (...)`; `NOT IN` always gets the `IS NULL` branch, because
    // `x NOT IN (...)` is not true for a NULL `x`.
    assert_eq!(sql("[i] IN (1, 2)", "postgres"), "i IN (1, 2)");
    assert_eq!(
        sql("[i] NOT IN (1, 2)", "postgres"),
        "(i NOT IN (1, 2)) OR i IS NULL"
    );
    // `NULL` cannot be compared with `=`, so it is moved into an `IS NULL` branch.
    assert_eq!(sql("[i] IN (1, NULL)", "postgres"), "i IS NULL OR i IN (1)");
    assert_eq!(
        sql("[i] NOT IN (1, NULL)", "postgres"),
        "(i NOT IN (1)) AND i IS NOT NULL"
    );
    // The fix-up is built with SQLAlchemy's `is_(None)`, which prints `IS NULL` in every
    // dialect — unlike a top-level `IS NULL`.
    assert_eq!(sql("[i] IN (1, NULL)", "mysql"), "i IS NULL OR i IN (1)");
    assert_eq!(
        sql("[i] IN (1, NULL)", "clickhouse"),
        "i IS NULL OR i IN (1)"
    );
    assert_eq!(
        sql("[i] NOT IN (1, NULL)", "mysql"),
        "(i NOT IN (1)) AND i IS NOT NULL"
    );
    // An empty list is the degenerate case of the same fix-up.
    assert_eq!(sql("[i] IN ()", "postgres"), "i IN (NULL) AND (1 != 1)");
    assert_eq!(
        sql("[i] NOT IN ()", "mysql"),
        "(i NOT IN (NULL) OR (1 = 1)) OR i IS NULL"
    );
    assert_eq!(sql("[i] IN (NULL)", "postgres"), "i IS NULL");
    assert_eq!(sql("[i] NOT IN (NULL)", "clickhouse"), "i IS NOT NULL");
}

#[test]
fn not_is_pushed_down_into_comparisons_like_sqlalchemy() {
    // `NOT (i = 1)` is not a `NOT` at all: SQLAlchemy flips the comparison operator.
    assert_eq!(sql("NOT ([i] = 1)", "postgres"), "i != 1");
    assert_eq!(sql("NOT ([i] <> 1)", "postgres"), "i = 1");
    assert_eq!(sql("NOT ([i] < 1)", "postgres"), "i >= 1");
    assert_eq!(sql("NOT ([s] LIKE 'a%')", "postgres"), "s NOT LIKE 'a%'");
    // Comparisons have no opposite, so they are negated as a whole.
    assert_eq!(
        sql("NOT ([b] AND [i] > 1)", "postgres"),
        "NOT (b AND i > 1)"
    );
    assert_eq!(sql("NOT [b]", "postgres"), "NOT b");
    // `<date>` is folded into a constant, and `NOT` of a non-boolean literal is `= 0`.
    assert_eq!(sql("NOT 1", "postgres"), "1 = 0");
    assert_eq!(sql("NOT 0", "clickhouse"), "0 = 0");
}

#[test]
fn not_in_is_expanded_back_from_its_fix_up() {
    // `NOT IN` renders as a boolean term, so its negation needs parentheses.
    assert_eq!(
        sql("NOT ([i] NOT IN (1, 2))", "postgres"),
        "NOT ((i NOT IN (1, 2)) OR i IS NULL)"
    );
    // ... except when the whole term collapsed to `IS NOT NULL`: MySQL renders that as
    // `NOT ISNULL()` and does not collapse the double negation.
    assert_eq!(sql("NOT ([i] NOT IN (NULL))", "postgres"), "i IS NULL");
    assert_eq!(sql("NOT ([i] IS NOT NULL)", "postgres"), "i IS NULL");
    assert_eq!(sql("NOT ([i] IS NOT NULL)", "mysql"), "NOT (NOT ISNULL(i))");
    assert_eq!(
        sql("NOT ([i] IS NOT NULL)", "clickhouse"),
        "NOT (NOT isNull(i))"
    );
}

#[test]
fn is_null_accepts_every_type_like_the_dummy_matcher() {
    // `isnull` is `V(D.DUMMY, ...)` in `dl_formula`: it accepts any type, so it must not
    // depend on the generated templates, which only cover the realistic types.
    let mut types = FieldTypes::new();
    types.insert("g".to_string(), DataType::Geopoint);
    types.insert("t".to_string(), DataType::TreeStr);
    let compile =
        |formula: &str, dialect: &str| compile_formula(formula, dialect, &types).unwrap().sql;
    assert_eq!(compile("[g] IS NULL", "postgres"), "g IS NULL");
    assert_eq!(compile("[t] IS NULL", "mysql"), "ISNULL(t)");
    assert_eq!(compile("[t] IS NOT NULL", "clickhouse"), "NOT isNull(t)");
    // The `IS NULL` branch of the `IN` fix-up prints `IS [NOT] NULL` in every dialect.
    assert_eq!(compile("[g] NOT IN (NULL)", "postgres"), "g IS NOT NULL");
    assert_eq!(compile("[t] IN (NULL)", "mysql"), "t IS NULL");
}

#[test]
fn is_true_on_dates_is_a_constant() {
    // `istrue`/`isfalse` of a date are constants: `true`/`false`, `1`/`0` in ClickHouse.
    // Negation is `<literal> = 0` in MySQL and `NOT <literal>` elsewhere — a literal needs
    // no parentheses, unlike a bare operand.
    assert_eq!(sql("[d] IS TRUE", "postgres"), "true");
    assert_eq!(sql("[d] IS FALSE", "clickhouse"), "0");
    assert_eq!(sql("[dt] IS TRUE", "clickhouse"), "1");
    assert_eq!(sql("[d] IS NOT TRUE", "postgres"), "NOT true");
    assert_eq!(sql("[d] IS NOT FALSE", "mysql"), "false = 0");
    assert_eq!(sql("[dt] IS NOT TRUE", "clickhouse"), "NOT 1");
}

#[test]
fn istrue_and_isfalse_are_the_is_operator() {
    // `dl_formula` translates `ISTRUE`/`ISFALSE` with `value.is_(True)`/`value.is_(False)`
    // (`operators_unary.UnaryIsTrue*`), which is the `IS TRUE`/`IS FALSE` spelling: a boolean
    // keeps the literal, a number compares with zero, a string with `''`, and a date is a
    // constant. ClickHouse has no boolean type, so a boolean compares with zero there.
    assert_eq!(sql("ISTRUE([b])", "postgres"), "b IS true");
    assert_eq!(sql("ISFALSE([b])", "postgres"), "b IS false");
    assert_eq!(sql("ISTRUE([b])", "clickhouse"), "b != 0");
    assert_eq!(sql("ISTRUE([i])", "postgres"), "i != 0.0");
    assert_eq!(sql("ISFALSE([f])", "postgres"), "f = 0.0");
    assert_eq!(sql("ISTRUE([s])", "postgres"), "s != ''");
    assert_eq!(sql("ISFALSE([s])", "postgres"), "s = ''");
    assert_eq!(sql("ISTRUE([d])", "postgres"), "true");
    assert_eq!(sql("ISFALSE([d])", "clickhouse"), "0");
    // The result is a condition, not a parenthesized expression: it composes with `AND`.
    assert_eq!(sql("ISTRUE([i]) AND [b]", "postgres"), "i != 0.0 AND b");

    // The argument types `dl_formula` has no variant for are rejected, not guessed at.
    let mut types = FieldTypes::new();
    types.insert("t".to_string(), DataType::TreeStr);
    types.insert("tz".to_string(), DataType::Datetimetz);
    let err = compile_formula("ISTRUE([t])", "postgres", &types).unwrap_err();
    assert!(err.contains("IS TRUE is not defined for TREE_STR"), "{err}");
    let err = compile_formula("ISFALSE([tz])", "postgres", &types).unwrap_err();
    assert!(
        err.contains("IS FALSE is not defined for DATETIMETZ"),
        "{err}"
    );
    assert!(compile_formula("ISTRUE(NULL)", "postgres", &types).is_err());
}

#[test]
fn db_cast_uses_the_connector_whitelist() {
    // `FuncDbCastBase`: the type name is matched case-insensitively against the connector's
    // whitelist for the argument type, and the params are validated as `CONST_INTEGER`.
    // PostgreSQL spells the SQLAlchemy type, ClickHouse adds the `Nullable(...)` wrapper.
    assert_eq!(
        sql(r#"DB_CAST([f], "numeric", 10, 5)"#, "postgres"),
        "CAST(f AS NUMERIC(10, 5))"
    );
    assert_eq!(
        sql(r#"DB_CAST([f], "double precision")"#, "postgres"),
        "CAST(f AS DOUBLE PRECISION)"
    );
    assert_eq!(
        sql(r#"DB_CAST([i], "BigInt")"#, "postgres"),
        "CAST(i AS BIGINT)"
    );
    // The `char`/`varchar` aliases are in the reference whitelist too.
    assert_eq!(
        sql(r#"DB_CAST([s], "varchar", 10)"#, "postgres"),
        "CAST(s AS VARCHAR(10))"
    );
    assert_eq!(
        sql(r#"DB_CAST([f], "decimal", 10, 5)"#, "clickhouse"),
        "CAST(f AS Nullable(Decimal(10, 5)))"
    );

    // A native type outside the whitelist, a source type with no whitelist and a wrong param
    // count are translation errors, exactly as in the reference.
    let err = compile_formula(r#"DB_CAST([f], "money")"#, "postgres", &fields()).unwrap_err();
    assert!(err.contains("Native data type MONEY"), "{err}");
    let err = compile_formula(r#"DB_CAST([d], "text")"#, "postgres", &fields()).unwrap_err();
    assert!(err.contains("Data type DATE is not supported"), "{err}");
    let err = compile_formula(r#"DB_CAST([f], "numeric")"#, "postgres", &fields()).unwrap_err();
    assert!(err.contains("requires 2 parameters"), "{err}");
    let err =
        compile_formula(r#"DB_CAST([f], "numeric", 10.5, 2)"#, "postgres", &fields()).unwrap_err();
    assert!(err.contains("invalid type CONST_FLOAT"), "{err}");
    let err = compile_formula("DB_CAST([f], 5)", "postgres", &fields()).unwrap_err();
    assert!(err.contains("string literal"), "{err}");
}

#[test]
fn at_date_forks_like_ago_but_joins_on_the_date_argument() {
    // `LookupFunctionToQueryForkMutation`/`AtDateLookupFunctionMutator`: the result of the
    // call is the measure, and the fork is joined on `date_argument = fork.<date dimension>`
    // (no `DATEADD` offset). The measure stays a leaf of the plan.
    let compiled = dfl_native::compile_leaves_from(
        &parse_formula("AT_DATE(SUM([f]), [d], #2020-01-01#)").unwrap(),
        dfl_native::Dialect::parse("postgres").unwrap(),
        &fields(),
        &FieldNames::default(),
        0,
        &dfl_native::WindowContext::default(),
    )
    .unwrap();
    let forked = compiled
        .leaves
        .iter()
        .position(|leaf| leaf.external.is_some())
        .expect("a lookup is a fork");
    assert_eq!(compiled.sql, format!("{{leaf:{forked}}}"));
    // The measure is a leaf of the plan too: the fork's own query selects it.
    assert_eq!(compiled.leaves[0].sql, "sum(f)");
    let external = compiled.leaves[forked]
        .external
        .as_ref()
        .expect("a lookup is a fork");
    assert_eq!(external.kind, dfl_native::ExternalKind::Lookup);
    let lookup = external.lookup.as_ref().expect("lookup conditions");
    // The dimension is a leaf of the plan too; the fork layer inlines it when it builds the join.
    assert_eq!(compiled.leaves[1].sql, "d");
    assert_eq!(lookup.dimension, "{leaf:1}");
    // The base side is the date argument, the fork side is the dimension itself.
    assert_eq!(lookup.base, "CAST('2020-01-01' AS DATE)");
    assert_eq!(lookup.offset, "{dim}");
    assert_eq!(lookup.day, None);

    // The type of the call is its measure's (`return_type = FromArgs(0)`), also without a plan.
    assert_eq!(
        dfl_native::infer_type(
            &parse_formula("AT_DATE(SUM([f]), [d], #2020-01-01#)").unwrap(),
            dfl_native::Dialect::parse("postgres").unwrap(),
            &fields(),
        )
        .unwrap(),
        DataType::Float
    );

    // Without a query plan there is no fork to build, and the error names the mutation that
    // is missing — `dl_formula` raises "Lookup function should have been mutated" here too.
    let err = compile_formula(
        "AT_DATE(SUM([f]), [d], #2020-01-01#)",
        "postgres",
        &fields(),
    )
    .unwrap_err();
    assert!(err.contains("should have been mutated"), "{err}");

    // `LOOKUP` is not a function of the reference at all, so it must say that rather than
    // report a missing template.
    let err = compile_formula("LOOKUP([f], [s])", "postgres", &fields()).unwrap_err();
    assert!(err.contains("LOOKUP is not a function"), "{err}");
}

#[test]
fn in_rejects_types_with_no_comparable_pair() {
    // `IN` matches `COMPARABLE_TYPES` (through `casts_to`): `DATETIMETZ` and `MARKUP` start
    // no pair there, so even an empty list is rejected — `dl_formula` raises
    // `TranslationError` for them too.
    let mut types = FieldTypes::new();
    types.insert("tz".to_string(), DataType::Datetimetz);
    types.insert("m".to_string(), DataType::Markup);
    types.insert("t".to_string(), DataType::TreeStr);
    assert!(compile_formula("[tz] IN ()", "postgres", &types).is_err());
    assert!(compile_formula("[m] IN (1, 2)", "postgres", &types).is_err());
    // A tree of strings `casts_to` an array of strings, but a string does not: the list
    // element decides, exactly like `ArgTypeSequence.match_arg_types`.
    assert!(compile_formula("[t] IN ('a')", "postgres", &types).is_err());
    assert_eq!(
        compile_formula("[t] IN ([t])", "postgres", &types)
            .unwrap()
            .sql,
        "t IN (t)"
    );
}

#[test]
fn power_operator_is_supported() {
    assert_eq!(sql("[f] ^ 2", "postgres"), "POWER(f, 2)");
}

#[test]
fn physical_field_names_are_rendered() {
    let mut names = FieldNames::new();
    names.insert(
        "city".to_string(),
        vec!["avatar".to_string(), "city".to_string()].into(),
    );
    let mut types = FieldTypes::new();
    types.insert("city".to_string(), DataType::String);
    let compiled = compile_formula_named("[city]", "postgres", &types, &names).unwrap();
    assert_eq!(compiled.sql, "avatar.city");
}

#[test]
fn a_substituted_expression_is_rendered_verbatim() {
    // `dl_query_processing` replaces an aggregated field reference with the aggregate call
    // itself, which is why a name may hold raw SQL and not only a physical column.
    let mut names = FieldNames::new();
    names.insert(
        "sales".to_string(),
        FieldName::Sql("SUM(t0.\"Sales\")".to_string()),
    );
    let mut types = FieldTypes::new();
    types.insert("sales".to_string(), DataType::Integer);
    let compiled = compile_formula_named("[sales] / 2", "postgres", &types, &names).unwrap();
    assert_eq!(
        compiled.sql,
        "CAST(SUM(t0.\"Sales\") AS DOUBLE PRECISION) / 2"
    );
}

#[test]
fn identifiers_that_need_quoting_are_quoted() {
    let mut types = FieldTypes::new();
    types.insert("Order".to_string(), DataType::String);
    assert_eq!(
        compile_formula("[Order]", "postgres", &types).unwrap().sql,
        "\"Order\""
    );
    assert_eq!(
        compile_formula("[Order]", "mysql", &types).unwrap().sql,
        "`Order`"
    );
    // SQLAlchemy quotes ClickHouse identifiers with `"` as well, and the native output has
    // to be interchangeable with the SQL the Python engine compiles.
    assert_eq!(
        compile_formula("[Order]", "clickhouse", &types)
            .unwrap()
            .sql,
        "\"Order\""
    );
}

#[test]
fn calls_folded_into_constants_by_the_registry() {
    // `dl_formula` renders `HOUR(<date>)` as the literal 0 and `NOT <date>` as false,
    // so the generated rows have no placeholders at all.
    for dialect in ["postgres", "mysql", "clickhouse"] {
        assert_eq!(sql("HOUR([d])", dialect), "0");
        assert_eq!(sql("MINUTE([d])", dialect), "0");
        assert_eq!(sql("SECOND([d])", dialect), "0");
    }
    assert_eq!(sql("NOT [d]", "postgres"), "false");
    assert_eq!(sql("NOT [d]", "mysql"), "false");
    // ClickHouse has no boolean literals: `dl_formula` renders the same fold as 0.
    assert_eq!(sql("NOT [d]", "clickhouse"), "0");
    // The same call on a DATETIME is a real translation, not a fold.
    assert_eq!(
        sql("HOUR([dt])", "postgres"),
        "CAST(DATE_PART('hour', dt) AS BIGINT)"
    );
}

#[test]
fn unsupported_formulas_report_an_error() {
    let err = compile_formula("[i] +", "postgres", &fields()).unwrap_err();
    assert!(
        err.contains("expected") || err.contains("unexpected"),
        "unexpected: {err}"
    );
    let err = compile_formula("[i]", "oracle", &fields()).unwrap_err();
    assert!(err.contains("oracle"), "unexpected: {err}");
    // a function the registry does not know must not produce SQL
    assert!(compile_formula("NO_SUCH_FUNCTION(1)", "postgres", &fields()).is_err());
}

#[test]
fn parse_reports_the_ast() {
    let ast = parse_formula("SUM([f]) / 2").unwrap();
    assert!(format!("{ast:?}").contains("SUM"));
}

#[test]
fn datetimetz_to_naive_names_the_missing_timezone() {
    // Python translates it (`timezone(tz, CAST(x AS TIMESTAMPTZ))`), but the timezone lives in
    // the argument's `data_type_params`, which this type model does not carry. A UTC guess would
    // change the value, so the error says what is missing.
    for dialect in ["postgres", "clickhouse"] {
        let err = compile_formula("DATETIMETZ_TO_NAIVE([dt])", dialect, &fields()).unwrap_err();
        assert!(err.contains("DATETIMETZ_TO_NAIVE"), "{err}");
        assert!(err.contains("timezone"), "{err}");
        assert!(err.contains("data_type_params"), "{err}");
    }
}

#[test]
fn make_nan_has_no_translation_in_the_reference_either() {
    // `dl_formula.definitions.functions_special.FuncMakeNan` declares no `variants`, so the
    // reference translator answers the same "no translation" for `_MAKE_NAN()`.
    for dialect in ["postgres", "clickhouse"] {
        let err = compile_formula("_MAKE_NAN()", dialect, &fields()).unwrap_err();
        assert!(err.contains("no translation for _MAKE_NAN()"), "{err}");
    }
}
