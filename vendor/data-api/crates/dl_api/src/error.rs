//! Errors in the shape `data-api` returns them:
//! `{"code": ..., "message": ..., "details": {...}, "debug": {...}}`.

use dl_core::CoreError;
use dl_query::QueryError;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    /// `RegularAPIErrorSchema.details`: structured, user-safe data (`db_message`, ...).
    pub details: Value,
    /// `RegularAPIErrorSchema.debug`: diagnostics that are still safe to show.
    pub debug: Value,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: json!({}),
            debug: json!({}),
        }
    }

    /// Attach the `details` object.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Attach the `debug` object.
    pub fn with_debug(mut self, debug: Value) -> Self {
        self.debug = debug;
        self
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(400, code, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "ERR.DS_API.DB.DATASET_NOT_FOUND", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "ERR.DS_API", message)
    }

    /// `dl_core.exc.DatabaseQueryError`: HTTP 400, `ERR.DS_API.DB[.<SUBTYPE>]`, and the
    /// driver's own text in `details.db_message`.
    ///
    /// The subtype is recovered from the executor error: PostgreSQL carries its SQLSTATE
    /// (`dl_connector_postgresql...error_transformer.py` maps `UndefinedTable` to
    /// `SourceDoesNotExist`, and so on), ClickHouse carries a text error. Anything the port
    /// cannot classify stays the base `ERR.DS_API.DB` with the Python default message.
    pub fn database(db_message: impl Into<String>) -> Self {
        let raw_message = db_message.into();
        let (subtype, message) = classify_db_error(&raw_message);
        let code = match subtype {
            Some(subtype) => format!("ERR.DS_API.DB.{subtype}"),
            None => "ERR.DS_API.DB".to_string(),
        };
        let db_message = driver_message(&raw_message);
        Self::new(400, code, message)
            .with_details(json!({ "db_message": db_message }))
            .with_debug(json!({ "db_message": db_message, "query": null }))
    }

    /// The body `data-api` sends for a rejected formula or an invalid dataset.
    pub fn formula(message: impl Into<String>) -> Self {
        Self::new(400, "ERR.DS_API.FORMULA.VALIDATION", message)
    }

    /// `dl_core.exc.ResultRowCountLimitExceededError`: a result asked for more than
    /// `row_count_hard_limit` rows.
    pub fn row_count_limit() -> Self {
        Self::new(
            400,
            "ERR.DS_API.ROW_COUNT_LIMIT",
            "Received too many result data rows.",
        )
    }
}

/// The subtype and default message for a driver error (`dl_core.exc.*`).
fn classify_db_error(db_message: &str) -> (Option<&'static str>, &'static str) {
    if let Some(state) = sqlstate(db_message) {
        let subtype = match state {
            "42P01" | "3F000" => Some("SOURCE_DOES_NOT_EXIST"),
            "42703" => Some("COLUMN_DOES_NOT_EXIST"),
            "42883" => Some("INVALID_FUNCTION"),
            "42601" | "42P07" | "42P10" => Some("INVALID_QUERY"),
            "22012" => Some("ZERO_DIVISION"),
            "22P02" => Some("CANNOT_PARSE"),
            "22003" => Some("NUMBER_OUT_OF_RANGE"),
            "28P01" | "28000" => Some("AUTHENTICATION_FAILED"),
            "08000" | "08003" | "08006" | "08001" => Some("SOURCE_CONNECT_ERROR"),
            _ => None,
        };
        if let Some(subtype) = subtype {
            return (Some(subtype), db_error_message(subtype));
        }
    }
    // `tokio_postgres` reports a connection problem without a SQLSTATE.
    if db_message.starts_with("connect:") {
        return (
            Some("SOURCE_CONNECT_ERROR"),
            db_error_message("SOURCE_CONNECT_ERROR"),
        );
    }
    // ClickHouse answers `Code: 60. DB::Exception: Table ... doesn't exist`.
    let lower = db_message.to_ascii_lowercase();
    let subtype = if lower.contains("doesn't exist") {
        Some("SOURCE_DOES_NOT_EXIST")
    } else if lower.contains("unknown identifier") || lower.contains("unknown column") {
        Some("COLUMN_DOES_NOT_EXIST")
    } else if lower.contains("unknown function") {
        Some("INVALID_FUNCTION")
    } else if lower.contains("syntax error") {
        Some("INVALID_QUERY")
    } else if lower.contains("division by zero") {
        Some("ZERO_DIVISION")
    } else if lower.contains("memory limit") {
        Some("MEMORY_LIMIT_EXCEEDED")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        Some("SOURCE_ERROR")
    } else {
        None
    };
    match subtype {
        Some(subtype) => (Some(subtype), db_error_message(subtype)),
        None => (None, "Database error."),
    }
}

/// The driver's own text, stripped of the executor wrapper: the Python connector stores the
/// asyncpg exception message (`division by zero`) in `details.db_message`, while this executor
/// reports `query: ERROR: division by zero (SQLSTATE 22012)`.
fn driver_message(db_message: &str) -> &str {
    let trimmed = db_message.trim();
    let without_prefix = trimmed.strip_prefix("query: ").unwrap_or(trimmed);
    let without_state = match without_prefix.rfind(" (SQLSTATE ") {
        Some(index) => &without_prefix[..index],
        None => without_prefix,
    };
    without_state.strip_prefix("ERROR: ").unwrap_or(without_state)
}

/// The SQLSTATE of a `dl_postgres::describe` message (`... (SQLSTATE 42P01)`).
fn sqlstate(message: &str) -> Option<&str> {
    let start = message.rfind("(SQLSTATE ")? + "(SQLSTATE ".len();
    let state = &message[start..];
    state
        .get(..5)
        .filter(|state| state.len() == 5 && state.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// `DLBaseException.default_message` of each `DatabaseQueryError` subtype.
fn db_error_message(subtype: &str) -> &'static str {
    match subtype {
        "SOURCE_DOES_NOT_EXIST" => "Data source (table) does not exist.",
        "COLUMN_DOES_NOT_EXIST" => "Requested database column does not exist.",
        "INVALID_FUNCTION" => "Unknown function.",
        "INVALID_QUERY" => "Invalid SQL query to the database.",
        "ZERO_DIVISION" => "Division by zero.",
        "CANNOT_PARSE" => "Cannot parse value.",
        "NUMBER_OUT_OF_RANGE" => "Number out of range.",
        "AUTHENTICATION_FAILED" => "Database authentication failed.",
        "SOURCE_CONNECT_ERROR" => "Data source refused connection.",
        "MEMORY_LIMIT_EXCEEDED" => "Memory limit has been exceeded during query execution.",
        "SOURCE_ERROR" => "Data source failed to respond correctly.",
        _ => "Database error.",
    }
}

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::UnknownDataType(_)
            | CoreError::UnknownUserDataType(_)
            | CoreError::EmptyGuid { .. }
            | CoreError::MissingDataType { .. }
            | CoreError::MissingFormula { .. }
            | CoreError::UnknownSource { .. }
            | CoreError::UnrenderableField { .. } => Self::formula(error.to_string()),
            _ => Self::bad_request("ERR.DS_API.DATASET", error.to_string()),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Core(inner) => inner.into(),
            QueryError::Formula { ref reason, .. } => {
                // `dl_formula`'s translation errors: the reference keeps the translation code
                // (`ERR.DS_API.FORMULA.TRANSLATION.UNKNOWN_FUNCTION` for a name the table does
                // not know) instead of the generic validation code.
                let lower = reason.to_ascii_lowercase();
                if reason.starts_with("Unknown ") && lower.contains("function or operator") {
                    // `unknown_function_reason` already produced the reference wording.
                    Self::bad_request(
                        "ERR.DS_API.FORMULA.TRANSLATION.UNKNOWN_FUNCTION",
                        reason.clone(),
                    )
                } else if lower.contains("unknown function") || lower.contains("no translation for") {
                    // `no translation for MOD(INTEGER|lit:int:2) in postgres`: the same class as
                    // `dl_formula`'s `UnknownFunctionError`, whose message is "Unknown 2-argument
                    // function or operator MOD" (`token`/`row`/`column` are part of the pack).
                    let name = reason
                        .split("no translation for ")
                        .nth(1)
                        .and_then(|rest| rest.split('(').next())
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .unwrap_or("function");
                    let arguments = reason
                        .split_once('(')
                        .map(|(_, rest)| {
                            let mut depth = 1usize;
                            let mut commas = 0usize;
                            let mut seen = false;
                            for character in rest.chars() {
                                match character {
                                    '(' => { depth += 1; seen = true; }
                                    ')' => { depth = depth.saturating_sub(1); if depth == 0 { break; } }
                                    ',' if depth == 1 => commas += 1,
                                    value if !value.is_whitespace() => seen = true,
                                    _ => {}
                                }
                            }
                            if seen { commas + 1 } else { 0 }
                        })
                        .unwrap_or(0);
                    Self::bad_request(
                        "ERR.DS_API.FORMULA.TRANSLATION.UNKNOWN_FUNCTION",
                        format!("Unknown {arguments}-argument function or operator {name}"),
                    )
                } else {
                    Self::formula(error.to_string())
                }
            }
            // `dl_core.exc.FieldNotFoundError`: `ERR.DS_API.FIELD.NOT_FOUND`,
            // `Unknown field <id>` — what the reference answers for a reference, a filter or an
            // `order_by` entry that names no field of the dataset.
            QueryError::UnknownField(_) => {
                Self::bad_request("ERR.DS_API.FIELD.NOT_FOUND", error.to_string())
            }
            // `dl_formula`'s `UnknownFieldInFormulaError`, the code the UI shows next to a field.
            QueryError::UnknownFieldInFormula(_) => Self::bad_request(
                "ERR.DS_API.FORMULA.UNKNOWN_FIELD_IN_FORMULA",
                error.to_string(),
            ),
            // `dl_query_processing.exc.MeasureFilterUnsupportedError`
            QueryError::MeasureFilterUnsupported { .. } => {
                Self::bad_request("ERR.DS_API.FILTER.MEASURE_UNSUPPORTED", error.to_string())
            }
            // Python raises a server error when RLS has no subject or a rule is unusable.
            QueryError::Rls(_) => Self::new(500, "ERR.DS_API.RLS", error.to_string()),
            _ => Self::bad_request("ERR.DS_API.REQUEST", error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_postgres_undefined_table_is_a_source_error() {
        let error = ApiError::database(
            "query: ERROR: relation \"public.nope\" does not exist (SQLSTATE 42P01)",
        );
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.DB.SOURCE_DOES_NOT_EXIST");
        assert_eq!(error.message, "Data source (table) does not exist.");
        assert!(error.details["db_message"]
            .as_str()
            .is_some_and(|message| message.contains("does not exist")));
        assert!(error.debug["db_message"].is_string());
    }

    #[test]
    fn an_unknown_driver_error_stays_the_base_database_code() {
        let error = ApiError::database("driver exploded");
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.DB");
        assert_eq!(error.message, "Database error.");
        assert_eq!(error.details["db_message"], "driver exploded");
    }

    #[test]
    fn a_syntax_error_and_a_missing_column_have_their_own_codes() {
        assert_eq!(
            ApiError::database("query: ERROR: syntax error (SQLSTATE 42601)").code,
            "ERR.DS_API.DB.INVALID_QUERY"
        );
        assert_eq!(
            ApiError::database("query: ERROR: column \"x\" does not exist (SQLSTATE 42703)").code,
            "ERR.DS_API.DB.COLUMN_DOES_NOT_EXIST"
        );
    }

    #[test]
    fn the_default_envelope_has_all_four_keys() {
        let error = ApiError::internal("boom");
        assert_eq!(error.details, json!({}));
        assert_eq!(error.debug, json!({}));
    }
}
