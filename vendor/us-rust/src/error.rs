//! Error model — mirrors `src/components/error-response-presenter.ts`,
//! `src/components/errors/define.ts` and the `VALIDATION_ERROR` contract of
//! `src/components/zod/index.ts`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

pub const US_AUTH_UNAUTHORIZED_ACCESS: &str = "US.AUTH.UNAUTHORIZED_ACCESS";

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: Option<&'static str>,
    pub message: String,
    pub details: Option<Value>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: Option<&'static str>, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// `NotExistEntryError` — 404 `NOT_EXIST_ENTRY`.
    pub fn not_exist_entry() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Some("NOT_EXIST_ENTRY"),
            "The entry doesn't exist",
        )
    }

    /// `NotExistTenantError` — 404.
    pub fn not_exist_tenant() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Some("NOT_EXIST_TENANT"),
            "The specified tenant doesn't exist",
        )
    }

    /// `US_ERRORS.TENANT_IS_BEING_DELETED` — 400.
    ///
    /// Thrown as a bare `AppError(US_ERRORS.TENANT_IS_BEING_DELETED, {code})`,
    /// so the message is the code itself and the presenter emits
    /// `{"code": "TENANT_IS_BEING_DELETED", "message": "TENANT_IS_BEING_DELETED"}`.
    pub fn tenant_is_being_deleted() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("TENANT_IS_BEING_DELETED"),
            "TENANT_IS_BEING_DELETED",
        )
    }

    /// `US_ERRORS.NOT_MATCH_TOGETHER` — 409.
    ///
    /// `error-response-presenter.ts` has a dedicated branch for this code which
    /// returns **only** a message (`{"message": "Not correct folderId"}`): no
    /// `code`, no `details`. Raised by `checkFetchedEntry` for entries without a
    /// workbook that belong to another tenant.
    pub fn not_match_together() -> Self {
        Self::new(StatusCode::CONFLICT, None, "Not correct folderId")
    }

    /// `WorkbookNotExistsError` — 404 `WORKBOOK_NOT_EXISTS`.
    pub fn workbook_not_exists() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Some("WORKBOOK_NOT_EXISTS"),
            "The workbook doesn't exist",
        )
    }

    /// `EntryAndWorkbookTenantMismatchError` — 500.
    pub fn entry_and_workbook_tenant_mismatch() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("ENTRY_AND_WORKBOOK_TENANT_MISMATCH"),
            "The entry and workbook belong to different tenants",
        )
    }

    /// `CollectionNotExistsError` — 404 `COLLECTION_NOT_EXISTS`.
    pub fn collection_not_exists() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            Some("COLLECTION_NOT_EXISTS"),
            "The collection doesn't exist",
        )
    }

    /// `EntryAndCollectionTenantMismatchError` — 500.
    pub fn entry_and_collection_tenant_mismatch() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("ENTRY_AND_COLLECTION_TENANT_MISMATCH"),
            "The entry and collection belong to different tenants",
        )
    }

    /// `CollectionEntryUnsupportedScopeError` — 400.
    ///
    /// The open-source registry plugin
    /// (`src/registry/plugins/common/entities/collection-entry/collection-entry.ts`)
    /// throws it from the constructor, i.e. unconditionally, for every entry that
    /// lives in a collection (no access to collection entries in this build).
    pub fn collection_entry_unsupported_scope() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("COLLECTION_ENTRY_UNSUPPORTED_SCOPE"),
            "Unsupported collection entry scope",
        )
    }

    /// `AccessServicePermissionDeniedError` — 403 `ACCESS_SERVICE_PERMISSION_DENIED`.
    ///
    /// Raised by `checkWorkbookPermission` / `checkCollectionEntryPermission`
    /// (`services/new/workbook/utils/check-workbook-permission.ts`).
    pub fn access_service_permission_denied() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            Some("ACCESS_SERVICE_PERMISSION_DENIED"),
            "Auth denied",
        )
    }

    /// `WorkbookIsolationInterruptionError` — 403.
    ///
    /// `services/new/entry/utils/check-workbook-isolation.ts`: the entry belongs
    /// to another workbook than the one named by `x-dl-workbookid`.
    pub fn workbook_isolation_interruption() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            Some("WORKBOOK_ISOLATION_INTERRUPTION"),
            "Workbook isolation interruption",
        )
    }

    /// `EntriesWithInsufficientPermissionsError` — 403 (folder delete with
    /// children the caller may not modify).
    pub fn entries_with_insufficient_permissions() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            Some("ENTRIES_WITH_INSUFFICIENT_PERMISSIONS"),
            "You can't do this action because of you don't have enough permissions for some entries",
        )
    }

    /// `ModeNotAllowedError` — 400 (`mode: 'recover'` on a non-recover route).
    pub fn mode_not_allowed() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("MODE_NOT_ALLOWED"),
            "This mode doesn't allowed",
        )
    }

    /// `CircularReferenceError` — 409 (moving workbooks/collections).
    pub fn circular_reference_error() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            Some("CIRCULAR_REFERENCE_ERROR"),
            "Circular reference error",
        )
    }

    /// `SyncLinksFailedError` — 500 (`Entry.syncLinks` failed).
    pub fn sync_links_failed() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("SYNC_LINKS_FAILED"),
            "Failed to sync entry links",
        )
    }

    /// `TENANT_ID_MISSING_IN_CONTEXT` — 400 (`checkCtxTenantId`).
    ///
    /// `error-response-presenter.ts` has a dedicated branch for this code and
    /// rewrites the `AppError` message.
    pub fn tenant_id_missing_in_context() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("TENANT_ID_MISSING_IN_CONTEXT"),
            "TenantId is missing. Probably it needs to be passed in the request headers.",
        )
    }

    /// `appAuth` middleware rejection — 401.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            Some(US_AUTH_UNAUTHORIZED_ACCESS),
            "Unauthorized access",
        )
    }

    /// `VALIDATION_ERROR` produced by a zod contract violation.
    pub fn validation(issues: Value) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            Some("VALIDATION_ERROR"),
            "Validation error",
        )
        .with_details(issues)
    }

    /// A single zod issue for an optional `zc.encodedId()` value which failed to
    /// decode (`z.string().length(13).transform(makeIdDecoder)`).
    ///
    /// `zod` reports the length constraint before the decoder runs, so a value
    /// of the wrong length yields `too_small`/`too_big`; only a 13 character
    /// value which fails `Utils.decodeId` yields the `custom` issue.
    ///
    /// The shape is the zod v4 one (`zod@^4.3.6`, used by us v1.196.0): size
    /// issues carry `origin`/`inclusive`/`exact` (no longer `type`) and the
    /// message reads `Too small: expected string to have >=13 characters`.
    pub(crate) fn encoded_id_issue(field: &str, value: &str) -> Value {
        let length = value.chars().count();
        let expected = crate::id::ENCODED_ID_LENGTH;

        if length == expected {
            json!({
                "code": "custom",
                "message": format!("id '{value}' has incorrect format"),
                "path": [field],
            })
        } else if length < expected {
            json!({
                "origin": "string",
                "code": "too_small",
                "minimum": expected,
                "inclusive": true,
                "exact": true,
                "path": [field],
                "message": format!(
                    "Too small: expected string to have >={expected} characters"
                ),
            })
        } else {
            json!({
                "origin": "string",
                "code": "too_big",
                "maximum": expected,
                "inclusive": true,
                "exact": true,
                "path": [field],
                "message": format!(
                    "Too big: expected string to have <={expected} characters"
                ),
            })
        }
    }

    /// Invalid `:entryId` path param — same issue zod would report for
    /// `params: z.object({entryId: zc.encodedId()})`.
    pub fn invalid_entry_id(value: &str) -> Self {
        Self::validation(json!([Self::encoded_id_issue("entryId", value)]))
    }

    /// Invalid `revId` query value (`revId: zc.encodedId().optional()`).
    pub fn invalid_query_id(field: &str, value: &str) -> Self {
        Self::validation(json!([Self::encoded_id_issue(field, value)]))
    }

    /// A single zod issue for `branch: z.enum(['saved', 'published'])`.
    ///
    /// zod v4 reports enum violations as `invalid_value` (with the accepted
    /// `values`) instead of v3's `invalid_enum_value`/`received`/`options`.
    pub(crate) fn branch_issue() -> Value {
        json!({
            "code": "invalid_value",
            "values": ["saved", "published"],
            "path": ["branch"],
            "message": "Invalid option: expected one of \"saved\"|\"published\"",
        })
    }

    /// Invalid `branch` query value (`z.enum(['saved', 'published'])`).
    pub fn invalid_branch() -> Self {
        Self::validation(json!([Self::branch_issue()]))
    }

    /// All issues of one `VALIDATION_ERROR`; `zod` collects them while parsing
    /// the query object, so several invalid values are reported at once.
    pub fn validation_issues(issues: Vec<Value>) -> Self {
        Self::validation(Value::Array(issues))
    }

    /// `DBError` branch of the presenter.
    pub fn database(code: Option<&str>) -> Self {
        match code {
            Some("23505") => Self::new(
                StatusCode::BAD_REQUEST,
                Some("ERR.US.DB.UNIQUE_VIOLATION"),
                "The entity already exists",
            ),
            Some("22003") => Self::new(
                StatusCode::BAD_REQUEST,
                None,
                "Wrong passed entryId (it can be in links)",
            ),
            _ => Self::new(StatusCode::INTERNAL_SERVER_ERROR, None, "Database error"),
        }
    }

    /// Fallback branch of the presenter: 500 `{"message": "Internal Server Error"}`.
    #[allow(dead_code)]
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            None,
            "Internal Server Error",
        )
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::Database(db_error) => ApiError::database(db_error.code().as_deref()),
            _ => {
                tracing::error!(error = %error, "database error");
                ApiError::database(None)
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            code: self.code,
            message: &self.message,
            details: self.details.as_ref(),
        };

        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    /// Serializes an error exactly as `finalRequestHandler` would send it.
    async fn present(error: ApiError) -> (u16, String) {
        let response = error.into_response();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("error body");

        (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
    }

    /// Payloads of `src/components/error-response-presenter.ts` and of the
    /// `definePresentableError` specs (`src/components/errors/*.ts`) as of
    /// us v1.196.0.
    #[tokio::test]
    async fn presentable_errors_match_node_payloads() {
        let cases = [
            (
                ApiError::not_exist_entry(),
                404,
                r#"{"code":"NOT_EXIST_ENTRY","message":"The entry doesn't exist"}"#,
            ),
            (
                ApiError::not_exist_tenant(),
                404,
                r#"{"code":"NOT_EXIST_TENANT","message":"The specified tenant doesn't exist"}"#,
            ),
            (
                ApiError::tenant_is_being_deleted(),
                400,
                r#"{"code":"TENANT_IS_BEING_DELETED","message":"TENANT_IS_BEING_DELETED"}"#,
            ),
            (
                ApiError::not_match_together(),
                409,
                r#"{"message":"Not correct folderId"}"#,
            ),
            (
                ApiError::workbook_not_exists(),
                404,
                r#"{"code":"WORKBOOK_NOT_EXISTS","message":"The workbook doesn't exist"}"#,
            ),
            (
                ApiError::collection_not_exists(),
                404,
                r#"{"code":"COLLECTION_NOT_EXISTS","message":"The collection doesn't exist"}"#,
            ),
            (
                ApiError::entry_and_workbook_tenant_mismatch(),
                500,
                r#"{"code":"ENTRY_AND_WORKBOOK_TENANT_MISMATCH","message":"The entry and workbook belong to different tenants"}"#,
            ),
            (
                ApiError::entry_and_collection_tenant_mismatch(),
                500,
                r#"{"code":"ENTRY_AND_COLLECTION_TENANT_MISMATCH","message":"The entry and collection belong to different tenants"}"#,
            ),
            (
                ApiError::access_service_permission_denied(),
                403,
                r#"{"code":"ACCESS_SERVICE_PERMISSION_DENIED","message":"Auth denied"}"#,
            ),
            (
                ApiError::workbook_isolation_interruption(),
                403,
                r#"{"code":"WORKBOOK_ISOLATION_INTERRUPTION","message":"Workbook isolation interruption"}"#,
            ),
            (
                ApiError::entries_with_insufficient_permissions(),
                403,
                r#"{"code":"ENTRIES_WITH_INSUFFICIENT_PERMISSIONS","message":"You can't do this action because of you don't have enough permissions for some entries"}"#,
            ),
            (
                ApiError::mode_not_allowed(),
                400,
                r#"{"code":"MODE_NOT_ALLOWED","message":"This mode doesn't allowed"}"#,
            ),
            (
                ApiError::circular_reference_error(),
                409,
                r#"{"code":"CIRCULAR_REFERENCE_ERROR","message":"Circular reference error"}"#,
            ),
            (
                ApiError::sync_links_failed(),
                500,
                r#"{"code":"SYNC_LINKS_FAILED","message":"Failed to sync entry links"}"#,
            ),
            (
                ApiError::tenant_id_missing_in_context(),
                400,
                r#"{"code":"TENANT_ID_MISSING_IN_CONTEXT","message":"TenantId is missing. Probably it needs to be passed in the request headers."}"#,
            ),
        ];

        for (error, status, body) in cases {
            assert_eq!(present(error).await, (status, body.to_string()));
        }
    }

    /// `zc.encodedId()` = `z.string().length(13).transform(Utils.decodeId)`:
    /// the length refinement runs first, so only a 13 character value that
    /// fails to decode yields the `custom` issue.
    ///
    /// Issue shapes are the zod v4 ones (`zod@^4.3.6`) observed on us v1.196.0.
    #[tokio::test]
    async fn encoded_id_issues_match_zod() {
        const TOO_SMALL: &str = r#"{"origin":"string","code":"too_small","minimum":13,"inclusive":true,"exact":true,"path":["%FIELD%"],"message":"Too small: expected string to have >=13 characters"}"#;
        const TOO_BIG: &str = r#"{"origin":"string","code":"too_big","maximum":13,"inclusive":true,"exact":true,"path":["%FIELD%"],"message":"Too big: expected string to have <=13 characters"}"#;
        const CUSTOM: &str = r#"{"code":"custom","message":"id 'zzzzzzzzzzzzz' has incorrect format","path":["%FIELD%"]}"#;
        const BRANCH: &str = r#"{"code":"invalid_value","values":["saved","published"],"path":["branch"],"message":"Invalid option: expected one of \"saved\"|\"published\""}"#;

        let with_field = |template: &str, field: &str| template.replace("%FIELD%", field);
        let validation = |issues: &[&str]| {
            format!(
                r#"{{"code":"VALIDATION_ERROR","message":"Validation error","details":[{}]}}"#,
                issues.join(",")
            )
        };

        let cases: Vec<(ApiError, Vec<String>)> = vec![
            (
                ApiError::invalid_entry_id("short"),
                vec![with_field(TOO_SMALL, "entryId")],
            ),
            (
                ApiError::invalid_entry_id("zzzzzzzzzzzzzz"),
                vec![with_field(TOO_BIG, "entryId")],
            ),
            (
                ApiError::invalid_entry_id("zzzzzzzzzzzzz"),
                vec![with_field(CUSTOM, "entryId")],
            ),
            (
                ApiError::invalid_query_id("revId", "bad"),
                vec![with_field(TOO_SMALL, "revId")],
            ),
            (ApiError::invalid_branch(), vec![BRANCH.to_string()]),
            (
                // `/v1/entries/:id` parses `branch` before `revId`, so both
                // issues are reported in schema declaration order.
                ApiError::validation_issues(vec![
                    ApiError::branch_issue(),
                    ApiError::encoded_id_issue("revId", "bad"),
                ]),
                vec![BRANCH.to_string(), with_field(TOO_SMALL, "revId")],
            ),
        ];

        for (error, details) in cases {
            let details: Vec<&str> = details.iter().map(String::as_str).collect();

            assert_eq!(present(error).await, (400, validation(&details)));
        }
    }
}
