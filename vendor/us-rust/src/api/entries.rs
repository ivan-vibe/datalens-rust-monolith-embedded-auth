//! `GET /v1/entries/:entryId` and `GET /v1/entries/:entryId/meta`.
//!
//! Ports of `src/services/new/entry/get-entry`, `get-entry-meta` and the
//! response models `get-entry-model.ts` / `entry-meta-model.ts`.

use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::api::entries_read::{issue_invalid_type, QVal, RawQuery as ParsedQuery};
use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions;
use crate::state::{AppState, PrivatePermissions};

/// Columns selected by `get-entry/constants.ts` joined with the revision.
const ENTRY_SQL: &str = r#"
SELECT
    e.entry_id          AS entry_id,
    e.scope::text       AS scope,
    e.type              AS entry_type,
    e.display_key       AS display_key,
    e.created_by        AS created_by,
    e.created_at        AS created_at,
    e.hidden            AS hidden,
    e.saved_id          AS saved_id,
    e.published_id      AS published_id,
    e.tenant_id         AS tenant_id,
    e.public            AS is_public,
    e.unversioned_data  AS unversioned_data,
    e.workbook_id       AS workbook_id,
    e.collection_id     AS collection_id,
    r.rev_id            AS rev_id,
    r.data              AS data,
    r.meta              AS meta,
    r.links             AS links,
    r.annotation        AS annotation,
    r.version           AS version,
    r.source_version    AS source_version,
    r.updated_by        AS updated_by,
    r.updated_at        AS updated_at,
    t.tenant_id         AS tenant_exists,
    t.deleting          AS tenant_deleting,
    t.features          AS tenant_features,
    t.settings          AS tenant_settings,
    w.tenant_id         AS workbook_tenant_id,
    w.deleted_at        AS workbook_deleted_at,
    c.tenant_id         AS collection_tenant_id,
    c.deleted_at        AS collection_deleted_at,
    EXISTS (
        SELECT 1 FROM favorites AS f
        WHERE f.entry_id = e.entry_id
          AND f.login = $5::text
          AND f.entity_type = 'entry'
    )                   AS is_favorite
FROM entries AS e
JOIN revisions AS r
    ON r.rev_id = CASE
        WHEN $2::bigint IS NOT NULL THEN $2::bigint
        WHEN $3::text = 'published' THEN e.published_id
        ELSE e.saved_id
    END
   AND r.entry_id = e.entry_id
LEFT JOIN tenants AS t ON t.tenant_id = e.tenant_id
LEFT JOIN workbooks AS w ON w.workbook_id = e.workbook_id
LEFT JOIN collections AS c ON c.collection_id = e.collection_id
WHERE e.entry_id = $1
  AND e.is_deleted = FALSE
  AND e.tenant_id = $4
LIMIT 1
"#;

/// `entry-meta-model.ts` only needs the revision `meta`/`annotation` (and
/// `data` for `dash` scopes). Note: like the original `getEntryMeta`, the query
/// is not tenant-scoped — access control happens after the read.
const ENTRY_META_SQL: &str = r#"
SELECT
    e.entry_id      AS entry_id,
    e.scope::text   AS scope,
    e.type          AS entry_type,
    e.display_key   AS display_key,
    e.saved_id      AS saved_id,
    e.published_id  AS published_id,
    e.tenant_id     AS tenant_id,
    e.workbook_id   AS workbook_id,
    e.collection_id AS collection_id,
    r.meta          AS meta,
    r.annotation    AS annotation,
    r.data          AS data,
    w.tenant_id     AS workbook_tenant_id,
    w.deleted_at    AS workbook_deleted_at
FROM entries AS e
JOIN revisions AS r
    ON r.rev_id = CASE
        WHEN $2::bigint IS NOT NULL THEN $2::bigint
        WHEN $3::text = 'published' THEN e.published_id
        ELSE e.saved_id
    END
   AND r.entry_id = e.entry_id
LEFT JOIN workbooks AS w ON w.workbook_id = e.workbook_id
WHERE e.entry_id = $1
  AND e.is_deleted = FALSE
LIMIT 1
"#;

#[derive(Debug, Default)]
pub struct GetEntryQuery {
    pub branch: Option<String>,
    /// Already decoded: `zc.encodedId()` validates and transforms in one step.
    pub rev_id: Option<i64>,
    pub include_permissions_info: Option<String>,
    pub include_links: Option<String>,
    pub include_service_plan: Option<String>,
    pub include_tenant_features: Option<String>,
    pub include_favorite: Option<String>,
    pub include_tenant_settings: Option<String>,
}

/// One optional string query field: anything but a single string (a `qs` array
/// for a repeated key, an object for `a[b]=c`) is a zod `invalid_type`.
fn query_string(query: &ParsedQuery, key: &str, issues: &mut Vec<Value>) -> Option<String> {
    match query.get(key) {
        None => None,
        Some(QVal::Str(value)) => Some(value.clone()),
        Some(other) => {
            issues.push(issue_invalid_type(
                "string",
                other.received(),
                serde_json::json!([key]),
            ));
            None
        }
    }
}

impl GetEntryQuery {
    /// `withContract` parses `req.query` with express' `qs` and the zod schema of
    /// `controllers/entries/get-entry.ts`.
    ///
    /// Repeated keys are parsed by `qs` into arrays, which the zod schema then
    /// rejects: `z.enum()` reports `invalid_value` (`branch`), and the
    /// `z.string()` derivations (`revId`, the `stringBoolean` flags) report
    /// `invalid_type` with `received: "array"`. Handing the query string to
    /// axum's `Query` instead would reject the duplicate before the US schema
    /// and answer with a plain-text extractor error, not `VALIDATION_ERROR`.
    fn parse(raw: Option<&str>, issues: &mut Vec<Value>) -> Self {
        let query = ParsedQuery::parse(raw);

        // `branch` is `z.enum(['saved', 'published']).optional()`: an array or an
        // object is `invalid_value`, the same issue an unknown string produces.
        let branch = match query.get("branch") {
            None => None,
            Some(QVal::Str(value)) if value == "saved" || value == "published" => {
                Some(value.clone())
            }
            Some(_) => {
                issues.push(ApiError::branch_issue());
                None
            }
        };

        let rev_id = match query_string(&query, "revId", issues) {
            Some(raw) => match decode_id(&raw) {
                Ok(rev_id) => Some(rev_id),
                Err(_) => {
                    issues.push(ApiError::encoded_id_issue("revId", &raw));
                    None
                }
            },
            None => None,
        };

        Self {
            branch,
            rev_id,
            include_permissions_info: query_string(&query, "includePermissionsInfo", issues),
            include_links: query_string(&query, "includeLinks", issues),
            include_service_plan: query_string(&query, "includeServicePlan", issues),
            include_tenant_features: query_string(&query, "includeTenantFeatures", issues),
            include_favorite: query_string(&query, "includeFavorite", issues),
            include_tenant_settings: query_string(&query, "includeTenantSettings", issues),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EntryRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    hidden: bool,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    tenant_id: Option<String>,
    is_public: bool,
    unversioned_data: Option<Value>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    rev_id: i64,
    data: Option<Value>,
    meta: Option<Value>,
    links: Option<Value>,
    annotation: Option<Value>,
    version: Option<i32>,
    source_version: Option<i32>,
    updated_by: String,
    updated_at: DateTime<Utc>,
    tenant_exists: Option<String>,
    tenant_deleting: Option<bool>,
    tenant_features: Option<Value>,
    tenant_settings: Option<Value>,
    workbook_tenant_id: Option<String>,
    workbook_deleted_at: Option<DateTime<Utc>>,
    collection_tenant_id: Option<String>,
    collection_deleted_at: Option<DateTime<Utc>>,
    is_favorite: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct EntryMetaRow {
    entry_id: i64,
    scope: String,
    entry_type: String,
    display_key: Option<String>,
    saved_id: Option<i64>,
    published_id: Option<i64>,
    tenant_id: Option<String>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    meta: Option<Value>,
    annotation: Option<Value>,
    data: Option<Value>,
    workbook_tenant_id: Option<String>,
    workbook_deleted_at: Option<DateTime<Utc>>,
}

/// Response model of `get-entry-model.ts` (field order preserved).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetEntryResponse {
    pub entry_id: String,
    pub scope: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unversioned_data: Option<Value>,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
    pub saved_id: Option<String>,
    pub published_id: Option<String>,
    pub rev_id: String,
    pub tenant_id: Option<String>,
    pub data: Option<Value>,
    pub meta: Option<Value>,
    pub annotation: Option<Value>,
    pub version: Option<i32>,
    pub source_version: Option<i32>,
    pub hidden: bool,
    #[serde(rename = "public")]
    pub is_public: bool,
    pub workbook_id: Option<String>,
    pub collection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_permissions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_features: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_settings: Option<Value>,
}

/// Response model of `entry-meta-model.ts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryMetaResponse {
    pub entry_id: String,
    pub scope: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub key: Option<String>,
    pub meta: Option<Value>,
    pub annotation: Option<Value>,
    pub saved_id: Option<String>,
    pub published_id: Option<String>,
    pub tenant_id: Option<String>,
    pub workbook_id: Option<String>,
    pub collection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_description: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_description: Option<Value>,
}

fn parse_branch(value: Option<&str>) -> Result<&'static str, ApiError> {
    match value {
        None | Some("saved") => Ok("saved"),
        Some("published") => Ok("published"),
        Some(_) => Err(ApiError::invalid_branch()),
    }
}

/// `zc.stringBoolean()` = `z.string().toLowerCase().transform(isTrueArg)`
/// with `TRUE_FLAGS = ['1', 'true', true]` (`src/const/common.ts`), so the
/// value is lower-cased and matched exactly (no trimming) against `1`/`true`.
fn parse_flag(value: Option<&String>) -> bool {
    matches!(
        value.map(|raw| raw.to_lowercase()).as_deref(),
        Some("true") | Some("1")
    )
}

/// Node serializes `Date` as `toISOString()`: UTC, milliseconds, `Z` suffix.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Mirrors the `if (data.data.supportDescription)` guard of
/// `entry-meta-model.ts`: JavaScript truthiness decides whether the field is
/// reported at all, and the raw value is passed through unchanged.
fn json_truthy_field(data: &Option<Value>, field: &str) -> Option<Value> {
    let value = data.as_ref()?.get(field)?;

    let is_truthy = match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    };

    is_truthy.then(|| value.clone())
}

/// `GET /v1/entries/:entryId`
pub async fn get_entry(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
    user: Option<Extension<AuthUser>>,
) -> Result<Response, ApiError> {
    // express never routes an empty `:entryId` segment (`/v1/entries//meta`
    // falls through to `finalhandler`); axum captures it as `""`.
    if raw_entry_id.is_empty() {
        return Ok(crate::api::not_found::response(&method, uri.path()));
    }

    // `withContract` (expresskit) validates `params` and `query` separately and
    // merges both issue lists into a single `VALIDATION_ERROR`, params first:
    // `/v1/entries/short?branch=nope` reports the `entryId` issue *and* the
    // `branch` one. Query issues keep the schema declaration order
    // (`branch` before `revId`).
    let mut issues = Vec::new();

    // `zc.encodedId()` on the path param: the length check runs before the
    // decoder, so a wrong length never reaches `Utils.decodeId`.
    let entry_id = match decode_id(&raw_entry_id) {
        Ok(entry_id) => Some(entry_id),
        Err(_) => {
            issues.push(ApiError::encoded_id_issue("entryId", &raw_entry_id));
            None
        }
    };

    // `qs` parses `?branch=a&branch=b` into an array before the zod schema runs;
    // the parser reports the same `VALIDATION_ERROR` issues the original does.
    let query = GetEntryQuery::parse(uri.query(), &mut issues);

    let branch = match query.branch.as_deref() {
        Some("published") => "published",
        _ => "saved",
    };

    let rev_id = query.rev_id;

    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let entry_id = entry_id.expect("validated above");

    let include_permissions_info = parse_flag(query.include_permissions_info.as_ref());
    let include_links = parse_flag(query.include_links.as_ref());
    let include_tenant_features = parse_flag(query.include_tenant_features.as_ref());
    let include_favorite = parse_flag(query.include_favorite.as_ref());
    let include_tenant_settings = parse_flag(query.include_tenant_settings.as_ref());

    // `getServicePlan` of the open-source registry always returns undefined, so
    // `servicePlan` is never present in the response. The query parameter is
    // still accepted (and validated) for API compatibility.
    let _include_service_plan = parse_flag(query.include_service_plan.as_ref());

    // The favorite flag is fetched in the same statement (`EXISTS` on the
    // favorites primary key): one round-trip instead of two.
    let login = if include_favorite {
        Some(
            user.as_ref()
                .and_then(|Extension(user)| user.user.login.clone())
                .unwrap_or_else(|| "unknown".to_owned()),
        )
    } else {
        None
    };

    let row: Option<EntryRow> = sqlx::query_as::<_, EntryRow>(ENTRY_SQL)
        .bind(entry_id)
        .bind(rev_id)
        .bind(branch)
        .bind(state.config.tenant_id_override.as_str())
        .bind(login)
        .fetch_optional(&state.db.replica)
        .await?;

    let row = row.ok_or_else(ApiError::not_exist_entry)?;
    let is_favorite = if include_favorite {
        Some(row.is_favorite)
    } else {
        None
    };

    if row.tenant_exists.is_none() {
        return Err(ApiError::not_exist_tenant());
    }

    if row.tenant_deleting.unwrap_or(false) {
        return Err(ApiError::tenant_is_being_deleted());
    }

    // `get-entry/index.ts`: an entry living in a workbook/collection must have a
    // live parent of the same tenant, otherwise a 404/500 is reported before
    // permissions are resolved.
    if row.workbook_id.is_some() {
        if row.workbook_tenant_id.is_none() || row.workbook_deleted_at.is_some() {
            return Err(ApiError::workbook_not_exists());
        }

        if row.workbook_tenant_id.as_deref() != row.tenant_id.as_deref() {
            return Err(ApiError::entry_and_workbook_tenant_mismatch());
        }
    } else if row.collection_id.is_some() {
        if row.collection_tenant_id.is_none() || row.collection_deleted_at.is_some() {
            return Err(ApiError::collection_not_exists());
        }

        if row.collection_tenant_id.as_deref() != row.tenant_id.as_deref() {
            return Err(ApiError::entry_and_collection_tenant_mismatch());
        }

        // `checkCollectionEntry` -> `createCollectionEntry`, whose open-source
        // implementation throws from the constructor.
        return Err(ApiError::collection_entry_unsupported_scope());
    }

    let private_permissions = PrivatePermissions::from_headers(&headers);

    // `checkWorkbookIsolation` is skipped on private/audit routes.
    let is_private_route = uri.path().starts_with("/private");
    if !is_private_route {
        permissions::check_workbook_isolation(&headers, row.workbook_id, row.collection_id)?;
    }

    // `getEntryPermissionsByWorkbook` for workbook entries; folder entries get
    // `makeEntryWithFullPermissions` (DLS is disabled in this build). Private
    // routes bypass the access service.
    let editor = is_private_route
        || permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let permissions = include_permissions_info.then(|| {
        if row.workbook_id.is_some() {
            permissions::entry_permissions(editor)
        } else {
            permissions::full_entry_permissions()
        }
    });

    // `checkFolderEntriesByPermission` with DLS enabled asks the DLS stub for
    // bulk permissions; the shipped stub returns `[]`, which drops the folder row
    // from the original list responses. Folder entries cannot be created through
    // this API and a DLS backend is absent from the port, so folder entries keep
    // `makeEntryWithFullPermissions` (the DLS-disabled behaviour).
    // shortcut: DLS folder visibility; upgrade together with a real DLS service.

    let response = GetEntryResponse {
        entry_id: encode_id(row.entry_id),
        scope: row.scope.clone(),
        entry_type: row.entry_type,
        key: row.display_key,
        unversioned_data: private_permissions
            .filter_unversioned_data(&row.scope, row.unversioned_data),
        created_by: row.created_by,
        created_at: format_timestamp(row.created_at),
        updated_by: row.updated_by,
        updated_at: format_timestamp(row.updated_at),
        saved_id: encode_id_opt(row.saved_id),
        published_id: encode_id_opt(row.published_id),
        rev_id: encode_id(row.rev_id),
        tenant_id: row.tenant_id,
        data: row.data,
        meta: row.meta,
        annotation: row.annotation,
        version: row.version,
        source_version: row.source_version,
        hidden: row.hidden,
        is_public: row.is_public,
        workbook_id: encode_id_opt(row.workbook_id),
        collection_id: encode_id_opt(row.collection_id),
        // `includeLinks` keeps the field even when the column is NULL: the JSON
        // contains `"links": null`, it is not omitted (verified against Node).
        links: if include_links {
            Some(row.links.unwrap_or(Value::Null))
        } else {
            None
        },
        is_favorite,
        permissions,
        full_permissions: None,
        service_plan: None,
        tenant_features: include_tenant_features
            .then_some(row.tenant_features)
            .flatten(),
        tenant_settings: include_tenant_settings
            .then_some(row.tenant_settings)
            .flatten(),
    };

    Ok(Json(response).into_response())
}

/// `GET /v1/entries/:entryId/meta`
pub async fn get_entry_meta(
    State(state): State<AppState>,
    Path(raw_entry_id): Path<String>,
    method: Method,
    uri: OriginalUri,
) -> Result<Response, ApiError> {
    // Same express routing quirk as `get_entry`: `//meta` has no route.
    if raw_entry_id.is_empty() {
        return Ok(crate::api::not_found::response(&method, uri.path()));
    }

    // `makeReqParser` parses `params` and `query` concurrently and rejects with
    // the first failure, so a bad `:entryId` always wins over a bad `branch`.
    let entry_id =
        decode_id(&raw_entry_id).map_err(|_| ApiError::invalid_entry_id(&raw_entry_id))?;

    // Same `qs`/zod handling as `get_entry`: a repeated `branch` is an array,
    // which `z.enum()` rejects with `invalid_value`.
    let raw_query = ParsedQuery::parse(uri.query());
    let branch_value = match raw_query.get("branch") {
        None => None,
        Some(QVal::Str(value)) if value == "saved" || value == "published" => Some(value.clone()),
        Some(_) => return Err(ApiError::invalid_branch()),
    };
    let branch = parse_branch(branch_value.as_deref())?;

    let row: Option<EntryMetaRow> = sqlx::query_as(ENTRY_META_SQL)
        .bind(entry_id)
        .bind(None::<i64>)
        .bind(branch)
        .fetch_optional(&state.db.replica)
        .await?;

    let row = row.ok_or_else(ApiError::not_exist_entry)?;

    // `get-entry-meta/index.ts`:
    //  - `checkFetchedEntry` runs for entries without a workbook and requires the
    //    entry tenant to match the context tenant (`NOT_MATCH_TOGETHER`);
    //  - entries living in a workbook go through `getWorkbook`, which selects
    //    only live workbooks of the context tenant (`WORKBOOK_NOT_EXISTS`);
    //  - collection entries go through `resolveCollectionEntryPermissions`,
    //    which builds the registry `CollectionEntry` entity — and its open-source
    //    implementation throws from the constructor.
    if row.workbook_id.is_some() {
        let workbook_tenant_id = row.workbook_tenant_id.as_deref();

        if workbook_tenant_id.is_none()
            || row.workbook_deleted_at.is_some()
            || workbook_tenant_id != Some(state.config.tenant_id_override.as_str())
        {
            return Err(ApiError::workbook_not_exists());
        }
    } else if row.collection_id.is_some() {
        return Err(ApiError::collection_entry_unsupported_scope());
    } else if row.tenant_id.as_deref() != Some(state.config.tenant_id_override.as_str()) {
        return Err(ApiError::not_match_together());
    }

    let (support_description, access_description) = if row.scope == "dash" {
        (
            json_truthy_field(&row.data, "supportDescription"),
            json_truthy_field(&row.data, "accessDescription"),
        )
    } else {
        (None, None)
    };

    let response = EntryMetaResponse {
        entry_id: encode_id(row.entry_id),
        scope: row.scope,
        entry_type: row.entry_type,
        key: row.display_key,
        meta: row.meta,
        annotation: row.annotation,
        saved_id: encode_id_opt(row.saved_id),
        published_id: encode_id_opt(row.published_id),
        tenant_id: row.tenant_id,
        workbook_id: encode_id_opt(row.workbook_id),
        collection_id: encode_id_opt(row.collection_id),
        support_description,
        access_description,
    };

    Ok(Json(response).into_response())
}

#[cfg(test)]
mod tests {
    use super::{parse_flag, GetEntryQuery};

    #[test]
    fn repeated_query_parameters_fail_like_qs_and_zod() {
        // `?branch=a&branch=b`: `qs` yields an array, `z.enum()` reports
        // `invalid_value` (not `invalid_type`, an enum rejects any non-member).
        let mut issues = Vec::new();
        let query = GetEntryQuery::parse(Some("branch=saved&branch=published"), &mut issues);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0]["code"], "invalid_value");
        assert_eq!(issues[0]["path"], serde_json::json!(["branch"]));
        assert_eq!(
            issues[0]["message"],
            "Invalid option: expected one of \"saved\"|\"published\""
        );
        assert!(query.branch.is_none());

        // The string flags and `revId` are `z.string()`: an array is
        // `invalid_type` with `received: "array"`.
        let mut issues = Vec::new();
        let query = GetEntryQuery::parse(
            Some("revId=a&revId=b&includeLinks=1&includeLinks=0"),
            &mut issues,
        );
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert_eq!(issues[0]["code"], "invalid_type");
        assert_eq!(issues[0]["path"], serde_json::json!(["revId"]));
        assert_eq!(issues[1]["path"], serde_json::json!(["includeLinks"]));
        assert_eq!(
            issues[1]["message"],
            "Invalid input: expected string, received array"
        );
        assert!(query.rev_id.is_none());
        assert!(query.include_links.is_none());

        // `branch[a]=b` becomes an object and is rejected the same way.
        let mut issues = Vec::new();
        GetEntryQuery::parse(Some("branch[a]=b"), &mut issues);
        assert_eq!(issues[0]["code"], "invalid_value");

        // Unknown repeated parameters are not part of the zod object: `qs`
        // parses them, zod strips them, the request proceeds (axum's `Query`
        // used to reject this with a duplicate-field error).
        let mut issues = Vec::new();
        let query = GetEntryQuery::parse(Some("unknown=1&unknown=2"), &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(query.branch.is_none());

        // The normal single-value form is unchanged.
        let valid_rev_id = "ch96cl4boozy1";
        let mut issues = Vec::new();
        let query = GetEntryQuery::parse(
            Some(&format!(
                "branch=published&revId={valid_rev_id}&includeLinks=true"
            )),
            &mut issues,
        );
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(query.branch.as_deref(), Some("published"));
        assert!(query.rev_id.is_some());
        assert_eq!(query.include_links.as_deref(), Some("true"));
    }

    #[test]
    fn parse_flag_matches_string_boolean() {
        // `TRUE_FLAGS` are lower-cased before the comparison, so every casing of
        // `true` and the literal `1` are accepted.
        for value in ["true", "TRUE", "True", "tRuE", "1"] {
            assert!(
                parse_flag(Some(&value.to_owned())),
                "{value} should be true"
            );
        }

        // Anything else is false — including non-`TRUE_FLAGS` words and values
        // that only look truthy after trimming (the JS implementation does not
        // trim).
        for value in ["false", "FALSE", "0", "yes", "on", "2", " true", "", " "] {
            assert!(
                !parse_flag(Some(&value.to_owned())),
                "{value} should be false"
            );
        }

        assert!(!parse_flag(None));
    }
}

// ---------------------------------------------------------------------------
// Direct (in-process) read model — used by the monolith's `src/us_direct.rs`.
//
// The chart path must not pay an engine -> US loopback HTTP/channel round trip for
// metadata. These functions run the same `ENTRY_SQL` against the replica pool the
// `us-rs` server already owns and return the `GET /private/entries/{id}` JSON shape,
// so callers can decode it exactly like a `us` response. Freshness is unchanged:
// there is no cache here, every call reads the current revision/branch.
// ---------------------------------------------------------------------------

/// One request-scoped US bundle: the chart entry (when the request names one), its
/// datasets and every connection the datasets point at.
#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsReadBundle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<Value>,
    pub datasets: Vec<Value>,
    pub connections: Vec<Value>,
}

/// Freshness token for a cross-request metadata cache.
///
/// An edit in US always writes a revision and moves `saved_id`/`published_id`; a delete (or a
/// hidden/updated row) moves `updated_at`. A cached entry carrying an older token is therefore
/// provably stale, so the monolith can serve `Arc<Value>` without a TTL guess and still never
/// answer with a pre-edit revision. `None` means "no live row" (deleted/missing): the caller
/// must not serve anything from the cache and should let `read_entry_json` produce the 404.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryFreshness {
    pub saved_id: Option<i64>,
    pub published_id: Option<i64>,
    pub updated_at_ms: i64,
}

/// Read the cheap freshness token of one entry (`saved_id`, `published_id`, `updated_at`).
///
/// One indexed row lookup on the replica; no `jsonb` decode and no revision read, so the cache
/// can validate every hit for a fraction of the cost of the read it replaces.
pub async fn entry_freshness(
    state: &AppState,
    raw_entry_id: &str,
) -> Result<Option<EntryFreshness>, ApiError> {
    let entry_id = decode_id(raw_entry_id).map_err(|_| ApiError::invalid_entry_id(raw_entry_id))?;
    let row: Option<(Option<i64>, Option<i64>, bool, i64)> = sqlx::query_as(
        "SELECT saved_id, published_id, is_deleted, \
         (EXTRACT(EPOCH FROM updated_at) * 1000)::bigint \
         FROM entries WHERE entry_id = $1",
    )
    .bind(entry_id)
    .fetch_optional(&state.db.replica)
    .await?;
    Ok(row.and_then(|(saved_id, published_id, is_deleted, updated_at_ms)| {
        (!is_deleted).then_some(EntryFreshness {
            saved_id,
            published_id,
            updated_at_ms,
        })
    }))
}

/// Read one entry and return the `GET /private/entries/{id}` JSON.
///
/// `branch` is `saved` or `published` (`published` when empty/unknown, matching the
/// engine's `DL_US_BRANCH` default). Like `UsClient::get_entry`, a published read that
/// finds no revision/data falls back to the `saved` branch, so a saved-but-unpublished
/// object (every demo connection) is still served.
pub async fn read_entry_json(
    state: &AppState,
    raw_entry_id: &str,
    branch: &str,
    rev_id: Option<&str>,
) -> Result<Value, ApiError> {
    let entry_id = decode_id(raw_entry_id).map_err(|_| ApiError::invalid_entry_id(raw_entry_id))?;
    let rev_id = match rev_id {
        Some(raw) => Some(decode_id(raw).map_err(|_| ApiError::invalid_entry_id(raw))?),
        None => None,
    };
    let branch = match branch {
        "saved" => "saved",
        _ => "published",
    };

    let first = read_entry_row_json(state, entry_id, rev_id, branch).await;
    let value = match first {
        Ok(value) => value,
        // A `published` miss may simply mean the object is saved but not published.
        Err(_) if branch == "published" => {
            return read_entry_row_json(state, entry_id, rev_id, "saved").await;
        }
        Err(error) => return Err(error),
    };

    // `follow_saved`: a published row with no revision data and no published pointer is
    // the saved-only case; read the saved branch instead.
    if branch == "published" {
        let no_data = value.get("data").map(Value::is_null).unwrap_or(true);
        let no_published = value.get("publishedId").map(Value::is_null).unwrap_or(true);
        if no_data && no_published {
            return read_entry_row_json(state, entry_id, rev_id, "saved").await;
        }
    }
    Ok(value)
}

/// One dataset/dataset/connection entry lookup, without the branch fallback.
async fn read_entry_row_json(
    state: &AppState,
    entry_id: i64,
    rev_id: Option<i64>,
    branch: &str,
) -> Result<Value, ApiError> {
    let row: Option<EntryRow> = sqlx::query_as::<_, EntryRow>(ENTRY_SQL)
        .bind(entry_id)
        .bind(rev_id)
        .bind(branch)
        .bind(state.config.tenant_id_override.as_str())
        .bind(Option::<String>::None)
        .fetch_optional(&state.db.replica)
        .await?;
    let row = row.ok_or_else(ApiError::not_exist_entry)?;

    // The internal read is the master-token path (engine/data-api): the entry exists
    // checks and the tenant guard are the same as `get_entry`, permissions are not
    // resolved here (the open-source defaults are all-true anyway).
    if row.tenant_exists.is_none() {
        return Err(ApiError::not_exist_tenant());
    }
    if row.tenant_deleting.unwrap_or(false) {
        return Err(ApiError::tenant_is_being_deleted());
    }

    let response = GetEntryResponse {
        entry_id: encode_id(row.entry_id),
        scope: row.scope,
        entry_type: row.entry_type,
        key: row.display_key,
        // Internal callers (dataset parse, connection secrets) need the secrets.
        unversioned_data: row.unversioned_data,
        created_by: row.created_by,
        created_at: format_timestamp(row.created_at),
        updated_by: row.updated_by,
        updated_at: format_timestamp(row.updated_at),
        saved_id: encode_id_opt(row.saved_id),
        published_id: encode_id_opt(row.published_id),
        rev_id: encode_id(row.rev_id),
        tenant_id: row.tenant_id,
        data: row.data,
        meta: row.meta,
        annotation: row.annotation,
        version: row.version,
        source_version: row.source_version,
        hidden: row.hidden,
        is_public: row.is_public,
        workbook_id: encode_id_opt(row.workbook_id),
        collection_id: encode_id_opt(row.collection_id),
        links: None,
        is_favorite: None,
        permissions: None,
        full_permissions: None,
        service_plan: None,
        tenant_features: None,
        tenant_settings: None,
    };
    serde_json::to_value(response).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("INTERNAL_ERROR"),
            format!("cannot serialize entry {entry_id}: {error}"),
        )
    })
}

/// `data.shared.datasetsIds` of a chart entry (the `shared` block is often a JSON string).
fn entry_dataset_ids(entry: &Value) -> Vec<String> {
    let data = match entry.get("data") {
        Some(value) => value,
        None => return Vec::new(),
    };
    let shared = data.get("shared");
    let shared = match shared {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        other => other.cloned(),
    };
    let Some(shared) = shared else {
        return Vec::new();
    };
    shared
        .get("datasetsIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `source_collections[].origin.connection_id` of a dataset entry (`dl_us::dataset`).
pub fn dataset_connection_id(entry: &Value) -> Option<String> {
    entry
        .pointer("/data/source_collections")
        .and_then(Value::as_array)?
        .iter()
        .filter(|collection| {
            collection
                .get("valid")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        })
        .find_map(|collection| {
            collection
                .pointer("/origin/connection_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
        })
}

/// One request-scoped bundle in a single call: chart entry (optional) + datasets +
/// their connections, all read from the replica pool. Dataset ids may be empty when
/// the entry is supplied: they are then taken from `entry.data.shared.datasetsIds`.
pub async fn read_bundle(
    state: &AppState,
    entry_id: Option<&str>,
    dataset_ids: &[String],
    branch: &str,
) -> Result<UsReadBundle, ApiError> {
    let entry = match entry_id {
        Some(id) => Some(read_entry_json(state, id, branch, None).await?),
        None => None,
    };

    let mut ids: Vec<String> = Vec::new();
    for id in dataset_ids {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        if let Some(entry) = &entry {
            for id in entry_dataset_ids(entry) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }

    let mut datasets = Vec::with_capacity(ids.len());
    let mut connection_ids: Vec<String> = Vec::new();
    for id in &ids {
        let dataset = read_entry_json(state, id, branch, None)
            .await
            .map_err(|_| ApiError::not_exist_entry())?;
        if let Some(connection_id) = dataset_connection_id(&dataset) {
            if !connection_ids.contains(&connection_id) {
                connection_ids.push(connection_id);
            }
        }
        datasets.push(dataset);
    }

    let mut connections = Vec::with_capacity(connection_ids.len());
    for id in &connection_ids {
        let connection = read_entry_json(state, id, branch, None)
            .await
            .map_err(|_| ApiError::not_exist_entry())?;
        connections.push(connection);
    }

    Ok(UsReadBundle {
        entry,
        datasets,
        connections,
    })
}
