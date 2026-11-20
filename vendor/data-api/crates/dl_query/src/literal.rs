//! Dialect-aware literals and identifier quoting.
//!
//! Identifiers follow the same rule the native SQL templates already follow (SQLAlchemy:
//! `"` for PostgreSQL/ClickHouse, backticks for MySQL), so a compiled query is a valid
//! data-api query for every supported backend.
//!
//! Literals are rendered as SQL text because the executor sends one statement. This keeps
//! the door open for bind parameters later: the renderer is the only place that decides.

use dl_core::DataType;

/// The SQL dialects the Rust backend can build a query for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    Postgres,
    Mysql,
    ClickHouse,
}

impl Dialect {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "postgresql_9_4" => Some(Self::Postgres),
            "mysql" | "mysql_8_0_12" => Some(Self::Mysql),
            "clickhouse" | "clickhouse_22_10" => Some(Self::ClickHouse),
            _ => None,
        }
    }

    /// The dialect a resolved connection DSN names (`clickhouse://...` -> `ClickHouse`).
    ///
    /// The scheme is what the connector resolver puts in front of the DSN, so a service that
    /// serves datasets from several databases compiles each request for its own database.
    pub fn from_dsn(dsn: &str) -> Option<Self> {
        let scheme = dsn.split("://").next()?.trim().to_ascii_lowercase();
        match scheme.as_str() {
            "clickhouse" | "clickhouses" => Some(Self::ClickHouse),
            "mysql" | "mysqls" => Some(Self::Mysql),
            "postgres" | "postgresql" => Some(Self::Postgres),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::ClickHouse => "clickhouse",
        }
    }

    fn quote_char(self) -> char {
        match self {
            Self::Postgres | Self::ClickHouse => '"',
            Self::Mysql => '`',
        }
    }

    /// `asc` + `NULLS FIRST`, as `dl_connector_postgresql` renders it.
    ///
    /// `QueryCompiler.force_nulls_lower_than_values` is set by the Postgres connector: NULLs
    /// sort before every value, which is not what `ASC` means there by default. The other
    /// dialects keep their own order, which is why this is a per-dialect string.
    pub fn nulls_first_asc(self) -> &'static str {
        match self {
            Self::Postgres => " NULLS FIRST",
            Self::Mysql | Self::ClickHouse => "",
        }
    }

    /// `sales` -> `"sales"` / `` `sales` ``, escaping the quote character itself.
    pub fn quote_ident(self, ident: &str) -> String {
        let quote = self.quote_char();
        let escaped = ident.replace(quote, &format!("{quote}{quote}"));
        format!("{quote}{escaped}{quote}")
    }

    /// `public.sales` -> `"public"."sales"`; an already-quoted part is left alone.
    pub fn quote_table(self, table: &str) -> String {
        table
            .split('.')
            .map(|part| {
                let trimmed = part.trim();
                if is_quoted(trimmed) {
                    trimmed.to_string()
                } else {
                    self.quote_ident(trimmed)
                }
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// `'...'` with the standard `''` escape. ClickHouse also honours backslash escapes, so
    /// the backslash has to be doubled there to keep the value intact.
    pub fn render_string(self, text: &str) -> String {
        let escaped = text.replace('\'', "''");
        if self == Self::ClickHouse {
            format!("'{}'", escaped.replace('\\', "\\\\"))
        } else {
            format!("'{escaped}'")
        }
    }

    /// A single SQL literal for `value`, typed as the field says.
    pub fn render_literal(
        self,
        value: &serde_json::Value,
        dtype: DataType,
    ) -> Result<String, LiteralError> {
        use serde_json::Value;

        if value.is_null() {
            return Ok("NULL".to_string());
        }
        if dtype.is_numeric() {
            return match value {
                Value::Number(number) => Ok(number.to_string()),
                // `data-api` accepts `"5"` for a numeric filter, so parse it the same way.
                Value::String(text) => text
                    .trim()
                    .parse::<f64>()
                    .map(|number| number.to_string())
                    .map_err(|_| LiteralError::NotANumber(text.clone())),
                _ => Err(LiteralError::NotANumber(value.to_string())),
            };
        }
        if dtype.is_datetime() {
            let text = value
                .as_str()
                .ok_or_else(|| LiteralError::NotATimestamp(value.to_string()))?;
            return Ok(self.render_timestamp(text, dtype));
        }
        match dtype.native().base() {
            dfl_native::DataType::Boolean => Ok(render_bool(value)?.to_string()),
            _ => Ok(self.render_string(&literal_to_text(value))),
        }
    }

    /// An array literal, the way `dl_formula` renders an array filter argument.
    ///
    /// `FuncStartswithArrayConst` compares the leading entries of the field with *the same*
    /// literal (`(x)[1:array_length(ARRAY['a'], 1)] = ARRAY['a']`), so the value has to be a
    /// typed list, not the JSON text `render_literal` falls back to for an unknown type.
    pub fn render_array_literal(self, value: &serde_json::Value) -> Result<String, LiteralError> {
        use serde_json::Value;

        let items = match value {
            Value::Array(items) => items,
            other => return Err(LiteralError::NotAnArray(literal_to_text(other))),
        };
        let rendered = items
            .iter()
            .map(|item| match item {
                Value::Null => Ok("NULL".to_string()),
                Value::Bool(flag) => Ok(if *flag { "TRUE" } else { "FALSE" }.to_string()),
                Value::Number(number) => Ok(number.to_string()),
                other => Ok(self.render_string(&literal_to_text(other))),
            })
            .collect::<Result<Vec<_>, LiteralError>>()?;
        Ok(match self {
            Self::Postgres => format!("ARRAY[{}]", rendered.join(", ")),
            Self::ClickHouse => format!("[{}]", rendered.join(", ")),
            // `FuncStartswithArrayConst` has no MySQL template — `dl_formula` has no array type
            // for it — so there is nothing to render the literal for.
            Self::Mysql => return Err(LiteralError::UnsupportedArray),
        })
    }

    fn render_timestamp(self, text: &str, dtype: DataType) -> String {
        // ClickHouse's `toDateTime` parses ISO with `T`, but not the trailing `Z` of a JSON
        // timestamp; the reference literalizer drops the zone for a `genericdatetime` too
        // (`toDateTime('2026-05-08T00:00:00')`).
        let text = match (self, dtype.native().base()) {
            (
                Self::ClickHouse,
                dfl_native::DataType::GenericDatetime | dfl_native::DataType::Datetime,
            ) => &clickhouse_datetime_text(text),
            _ => text,
        };
        let quoted = self.render_string(text);
        match self {
            // ClickHouse has no implicit string -> date cast in comparisons.
            Self::ClickHouse => {
                if dtype.native().base() == dfl_native::DataType::Date {
                    format!("toDate32({quoted})")
                } else {
                    format!("toDateTime({quoted})")
                }
            }
            // PostgreSQL/MySQL cast the string literal to the column type on comparison;
            // an explicit CAST keeps the plan (and the result) unambiguous.
            _ => format!("CAST({quoted} AS {})", sql_type_name(dtype)),
        }
    }
}

/// A JSON timestamp in the spelling ClickHouse's `toDateTime` parses and SQLAlchemy produces:
/// no trailing `Z`, and no zero fractional part (`2026-05-08T00:00:00`, not `...00.000`).
fn clickhouse_datetime_text(text: &str) -> String {
    let mut value = text.trim();
    if let Some(stripped) = value.strip_suffix('Z') {
        value = stripped;
    }
    // `toDateTime` takes whole seconds here: the reference formats `'%Y-%m-%d %H:%M:%S'`, so a
    // `...23:59:59.999` bound becomes `...23:59:59` (and `...00.000` is plain `...00`).
    if let Some(dot) = value.rfind('.') {
        if value[dot + 1..].chars().all(|digit| digit.is_ascii_digit()) {
            value = &value[..dot];
        }
    }
    value.to_string()
}

fn is_quoted(part: &str) -> bool {
    (part.starts_with('"') && part.ends_with('"') && part.len() > 1)
        || (part.starts_with('`') && part.ends_with('`') && part.len() > 1)
}

/// The SQL type a `CAST` needs for a data-api data type.
pub fn sql_type_name(dtype: DataType) -> &'static str {
    match dtype.native().base() {
        dfl_native::DataType::Integer => "BIGINT",
        dfl_native::DataType::Float => "DOUBLE PRECISION",
        dfl_native::DataType::Date => "DATE",
        dfl_native::DataType::Datetime | dfl_native::DataType::GenericDatetime => "TIMESTAMP",
        dfl_native::DataType::Datetimetz => "TIMESTAMPTZ",
        dfl_native::DataType::Boolean => "BOOLEAN",
        dfl_native::DataType::Uuid => "UUID",
        _ => "TEXT",
    }
}

/// A JSON value as text, the way `data-api` compares string fields.
pub(crate) fn literal_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn render_bool(value: &serde_json::Value) -> Result<bool, LiteralError> {
    match value {
        serde_json::Value::Bool(flag) => Ok(*flag),
        serde_json::Value::String(text) if text.eq_ignore_ascii_case("true") => Ok(true),
        serde_json::Value::String(text) if text.eq_ignore_ascii_case("false") => Ok(false),
        other => Err(LiteralError::NotABoolean(other.to_string())),
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LiteralError {
    #[error("value {0} is not a number")]
    NotANumber(String),
    #[error("value {0} is not a timestamp")]
    NotATimestamp(String),
    #[error("value {0} is not a boolean")]
    NotABoolean(String),
    #[error("value {0} is not an array")]
    NotAnArray(String),
    #[error("this dialect has no array literals")]
    UnsupportedArray,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identifiers_are_quoted_per_dialect_and_escape_their_quote() {
        assert_eq!(Dialect::Postgres.quote_ident("Sales"), "\"Sales\"");
        assert_eq!(Dialect::Mysql.quote_ident("Sales"), "`Sales`");
        assert_eq!(Dialect::Postgres.quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            Dialect::Postgres.quote_table("public.sales"),
            "\"public\".\"sales\""
        );
        assert_eq!(
            Dialect::Postgres.quote_table("\"public\".sales"),
            "\"public\".\"sales\""
        );
    }

    #[test]
    fn array_literals_are_lists_of_their_elements() {
        // `dl_formula` renders a `CONST_ARRAY_STR` argument as `ARRAY['a', 'b']` (PostgreSQL)
        // or `['a', 'b']` (ClickHouse); `FuncStartswithArrayConst` reuses it on both sides of
        // the comparison, so the element type has to survive.
        assert_eq!(
            Dialect::Postgres
                .render_array_literal(&json!(["Electronics"]))
                .unwrap(),
            "ARRAY['Electronics']"
        );
        assert_eq!(
            Dialect::Postgres
                .render_array_literal(&json!(["a", "it's", 5, null, true]))
                .unwrap(),
            "ARRAY['a', 'it''s', 5, NULL, TRUE]"
        );
        assert_eq!(
            Dialect::ClickHouse
                .render_array_literal(&json!(["a"]))
                .unwrap(),
            "['a']"
        );
        assert_eq!(
            Dialect::Postgres.render_array_literal(&json!("not a list")),
            Err(LiteralError::NotAnArray("not a list".to_string()))
        );
        // `dl_formula` has no array `startswith` for MySQL, so there is nothing to render for.
        assert_eq!(
            Dialect::Mysql.render_array_literal(&json!(["a"])),
            Err(LiteralError::UnsupportedArray)
        );
    }

    #[test]
    fn literals_are_typed_and_escaped() {
        let int = DataType::parse("INTEGER").unwrap();
        let string = DataType::parse("STRING").unwrap();
        let boolean = DataType::parse("BOOLEAN").unwrap();
        let date = DataType::parse("DATE").unwrap();

        assert_eq!(
            Dialect::Postgres.render_literal(&json!(5), int).unwrap(),
            "5"
        );
        assert_eq!(
            Dialect::Postgres.render_literal(&json!("5"), int).unwrap(),
            "5"
        );
        assert_eq!(
            Dialect::Postgres
                .render_literal(&json!("it's"), string)
                .unwrap(),
            "'it''s'"
        );
        assert_eq!(
            Dialect::Postgres
                .render_literal(&json!(true), boolean)
                .unwrap(),
            "true"
        );
        assert_eq!(
            Dialect::ClickHouse
                .render_literal(&json!("2020-01-01"), date)
                .unwrap(),
            "toDate32('2020-01-01')"
        );
        assert_eq!(
            Dialect::Postgres
                .render_literal(&json!("2020-01-01"), date)
                .unwrap(),
            "CAST('2020-01-01' AS DATE)"
        );
        assert!(Dialect::Postgres
            .render_literal(&json!("abc"), int)
            .is_err());
    }

    #[test]
    fn a_genericdatetime_filter_is_a_timestamp_literal_on_every_dialect() {
        // The charts engine sends `2026-05-08T00:00:00.000Z` for a `genericdatetime` field.
        // A bare string literal is not comparable with a ClickHouse `DateTime` column, and the
        // reference literalizer drops the `Z` (`toDateTime('2026-05-08T00:00:00')`).
        let generic = DataType::parse("GENERICDATETIME").unwrap();
        let value = json!("2026-05-08T00:00:00.000Z");
        assert_eq!(
            Dialect::ClickHouse.render_literal(&value, generic).unwrap(),
            "toDateTime('2026-05-08T00:00:00')"
        );
        // A non-zero fractional part is truncated too: `toDateTime` takes whole seconds.
        assert_eq!(
            Dialect::ClickHouse
                .render_literal(&json!("2026-11-02T23:59:59.999Z"), generic)
                .unwrap(),
            "toDateTime('2026-11-02T23:59:59')"
        );
        assert_eq!(
            Dialect::Postgres.render_literal(&value, generic).unwrap(),
            "CAST('2026-05-08T00:00:00.000Z' AS TIMESTAMP)"
        );
    }
}
