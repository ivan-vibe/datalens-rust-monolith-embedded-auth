//! `src/controllers/collections/*`, `src/controllers/structure-items/*`,
//! `src/controllers/collections/get-collection-content.ts`,
//! `src/services/new/collection/*` and `src/services/new/structure-item/*`.

use axum::extract::{OriginalUri, Path, State};
use axum::http::{StatusCode, Uri};
use axum::routing::{delete, get, post};
use axum::Extension;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use sqlx::{FromRow, PgPool};

use crate::api::workbooks::{
    build_cursor_clause, decode_page_token, encode_page_token, encoded_id_issue,
    entity_name_issues, escape_string_for_like, fetch_all, format_timestamp, issue_custom,
    issue_invalid_enum, issue_invalid_type, issue_too_big_number, issue_too_small_number,
    js_parse_int, parse_flag, validate_encoded_id_array, Bind, CursorField, QueryParams,
    TokenValidator,
};
use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::{decode_id, encode_id, encode_id_opt};
use crate::permissions;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        ("/v1/collections", post(create_collection)),
        (
            "/v1/collections/{collection_id}",
            get(get_collection).delete(delete_collection),
        ),
        (
            "/v1/collections/{collection_id}/breadcrumbs",
            get(breadcrumbs),
        ),
        (
            "/v1/collections/{collection_id}/update",
            post(update_collection),
        ),
        (
            "/v1/collections/{collection_id}/move",
            post(move_collection),
        ),
        (
            "/v1/collections-get-list-by-ids",
            post(get_collections_list_by_ids),
        ),
        ("/private/v1/collections", post(private_create_collection)),
        (
            "/private/v1/collections/{collection_id}",
            get(private_get_collection),
        ),
        ("/v1/collection-content", get(collection_content)),
        ("/v1/structure-items", get(structure_items)),
        ("/v1/root-collection-permissions", get(root_permissions)),
        ("/v1/delete-collections", delete(delete_collections)),
        ("/v1/move-collections", post(move_collections)),
    ])
}

// ---------------------------------------------------------------------------
// Collection model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub(crate) struct CollectionRow {
    pub collection_id: i64,
    pub title: String,
    pub description: Option<String>,
    pub parent_id: Option<i64>,
    pub tenant_id: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
    pub meta: Value,
    pub title_lower: String,
}

pub(crate) const COLLECTION_COLUMNS: &str = r#"
    collection_id,
    title,
    description,
    parent_id,
    tenant_id,
    created_by,
    created_at,
    updated_by,
    updated_at,
    meta,
    title_lower
"#;

#[allow(clippy::too_many_arguments)]
pub(crate) fn collection_json_parts(
    collection_id: i64,
    title: &str,
    description: Option<&str>,
    parent_id: Option<i64>,
    tenant_id: &str,
    created_by: &str,
    created_at: DateTime<Utc>,
    updated_by: &str,
    updated_at: DateTime<Utc>,
    meta: &Value,
    include_permissions: bool,
    editor: bool,
) -> Value {
    let mut map = Map::new();

    map.insert("collectionId".into(), json!(encode_id(collection_id)));
    map.insert("title".into(), json!(title));
    map.insert("description".into(), json!(description));
    map.insert("parentId".into(), json!(encode_id_opt(parent_id)));
    map.insert("tenantId".into(), json!(tenant_id));
    map.insert("createdBy".into(), json!(created_by));
    map.insert("createdAt".into(), json!(format_timestamp(created_at)));
    map.insert("updatedBy".into(), json!(updated_by));
    map.insert("updatedAt".into(), json!(format_timestamp(updated_at)));
    map.insert("meta".into(), meta.clone());

    if include_permissions {
        map.insert(
            "permissions".into(),
            permissions::collection_permissions(editor),
        );
    }

    Value::Object(map)
}

pub(crate) fn collection_json(
    model: &CollectionRow,
    include_permissions: bool,
    editor: bool,
) -> Value {
    collection_json_parts(
        model.collection_id,
        &model.title,
        model.description.as_deref(),
        model.parent_id,
        &model.tenant_id,
        &model.created_by,
        model.created_at,
        &model.updated_by,
        model.updated_at,
        &model.meta,
        include_permissions,
        editor,
    )
}

pub(crate) async fn fetch_collection(
    pool: &PgPool,
    tenant: &str,
    collection_id: i64,
) -> Result<Option<CollectionRow>, ApiError> {
    fetch_collection_impl(pool, Some(tenant), collection_id).await
}

pub(crate) async fn fetch_collection_impl(
    pool: &PgPool,
    tenant: Option<&str>,
    collection_id: i64,
) -> Result<Option<CollectionRow>, ApiError> {
    let sql = match tenant {
        Some(_) => format!(
            "SELECT {COLLECTION_COLUMNS} FROM collections \
             WHERE deleted_at IS NULL AND collection_id = $1 AND tenant_id = $2 LIMIT 1"
        ),
        None => format!(
            "SELECT {COLLECTION_COLUMNS} FROM collections \
             WHERE deleted_at IS NULL AND collection_id = $1 LIMIT 1"
        ),
    };

    let rows: Vec<CollectionRow> = match tenant {
        Some(tenant) => {
            fetch_all(
                pool,
                &sql,
                &[Bind::I64(collection_id), Bind::Str(tenant.to_owned())],
            )
            .await?
        }
        None => fetch_all(pool, &sql, &[Bind::I64(collection_id)]).await?,
    };

    Ok(rows.into_iter().next())
}

/// `getParents` — the recursive CTE with all live ancestors of the ids.
pub(crate) async fn get_parents(
    pool: &PgPool,
    tenant: &str,
    collection_ids: &[i64],
) -> Result<Vec<CollectionRow>, ApiError> {
    let sql = format!(
        "WITH RECURSIVE collectionParents AS ( \
            SELECT * FROM collections \
            WHERE tenant_id = $1 AND deleted_at IS NULL AND collection_id = ANY($2) \
            UNION \
            SELECT collections.* FROM collections \
            JOIN collectionParents \
              ON collectionParents.parent_id = collections.collection_id \
            WHERE collections.tenant_id = $1 AND collections.deleted_at IS NULL \
         ) \
         SELECT {COLLECTION_COLUMNS} FROM collectionParents"
    );

    fetch_all(
        pool,
        &sql,
        &[
            Bind::Str(tenant.to_owned()),
            Bind::I64List(collection_ids.to_vec()),
        ],
    )
    .await
}

pub(crate) async fn get_parent_ids(
    pool: &PgPool,
    tenant: &str,
    collection_ids: &[i64],
) -> Result<Vec<i64>, ApiError> {
    let parents = get_parents(pool, tenant, collection_ids).await?;
    Ok(parents
        .into_iter()
        .map(|parent| parent.collection_id)
        .collect())
}

async fn collection_title_exists(
    pool: &PgPool,
    tenant: &str,
    parent_id: Option<i64>,
    title_lower: &str,
) -> Result<bool, ApiError> {
    let sql = "SELECT 1 AS one FROM collections \
         WHERE tenant_id = $1 AND deleted_at IS NULL \
           AND parent_id IS NOT DISTINCT FROM $2 AND title_lower = $3 LIMIT 1";

    let rows: Vec<(i32,)> = fetch_all(
        pool,
        sql,
        &[
            Bind::Str(tenant.to_owned()),
            Bind::OptI64(parent_id),
            Bind::Str(title_lower.to_owned()),
        ],
    )
    .await?;

    Ok(!rows.is_empty())
}

// ---------------------------------------------------------------------------
// POST /v1/collections
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CreateCollectionBody {
    title: String,
    description: Option<String>,
    parent_id: Option<i64>,
}

fn validate_create_collection(body: &Value) -> Result<CreateCollectionBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name_value(body)),
        ])));
    };

    let mut parsed = CreateCollectionBody::default();

    match object.get("title") {
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = title.clone();
        }
        None => issues.push(issue_invalid_type(&["title"], "string", "undefined")),
        Some(other) => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name_value(other),
        )),
    }

    match object.get("description") {
        None => {}
        Some(Value::String(description)) => parsed.description = Some(description.clone()),
        Some(other) => issues.push(issue_invalid_type(
            &["description"],
            "string",
            received_name_value(other),
        )),
    }

    match object.get("parentId") {
        None => issues.push(issue_invalid_type(&["parentId"], "string", "undefined")),
        Some(Value::Null) => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => parsed.parent_id = Some(id),
            Err(_) => issues.push(encoded_id_issue("parentId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["parentId"],
            "string",
            received_name_value(other),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

fn received_name_value(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn create_collection(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    permissions::require_ctx_tenant_id(&state, false)?;
    permissions::require_editor(&state, user.as_ref().map(|Extension(user)| user))?;
    create_collection_impl(&state, &body, true).await
}

async fn private_create_collection(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    permissions::require_ctx_tenant_id(&state, true)?;
    create_collection_impl(&state, &body, true).await
}

async fn create_collection_impl(
    state: &AppState,
    body: &Value,
    editor: bool,
) -> Result<Json<Value>, ApiError> {
    let args = validate_create_collection(body)?;
    let tenant = state.config.tenant_id_override.clone();
    let title = args.title.trim().to_owned();
    let description = args.description.map(|value| value.trim().to_owned());

    if let Some(parent_id) = args.parent_id {
        let parents = get_parent_ids(&state.db.primary, &tenant, &[parent_id]).await?;

        if parents.is_empty() {
            return Err(ApiError::collection_not_exists().with_message(format!(
                "Cannot find parent collection with id – {}",
                encode_id(parent_id)
            )));
        }

        fetch_collection(&state.db.primary, &tenant, parent_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    }

    if collection_title_exists(
        &state.db.primary,
        &tenant,
        args.parent_id,
        &title.to_lowercase(),
    )
    .await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            Some("COLLECTION_ALREADY_EXISTS"),
            format!("Collection with title \"{title}\" already exists in this scope"),
        ));
    }

    let sql = format!(
        "INSERT INTO collections \
         (title, title_lower, description, parent_id, tenant_id, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING {COLLECTION_COLUMNS}"
    );

    let rows: Vec<CollectionRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str(title.clone()),
            Bind::Str(title.to_lowercase()),
            Bind::OptStr(description),
            Bind::OptI64(args.parent_id),
            Bind::Str(tenant),
            Bind::Str("systemId".to_owned()),
        ],
    )
    .await?;

    let model = rows.into_iter().next().ok_or_else(ApiError::internal)?;

    Ok(Json(collection_json(&model, false, editor)))
}

// ---------------------------------------------------------------------------
// GET /v1/collections/{collectionId}
// ---------------------------------------------------------------------------

async fn get_collection(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    get_collection_impl(&state, &raw_collection_id, &uri, false, editor).await
}

async fn private_get_collection(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, ApiError> {
    get_collection_impl(&state, &raw_collection_id, &uri, true, true).await
}

async fn get_collection_impl(
    state: &AppState,
    raw_collection_id: &str,
    uri: &Uri,
    is_private: bool,
    editor: bool,
) -> Result<Json<Value>, ApiError> {
    let collection_id = decode_id(raw_collection_id)
        .map_err(|_| ApiError::invalid_entry_id_for("collectionId", raw_collection_id))?;

    let query = QueryParams::parse(uri);
    let include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    let tenant = if is_private {
        None
    } else {
        Some(state.config.tenant_id_override.as_str())
    };

    let model = fetch_collection_impl(&state.db.replica, tenant, collection_id)
        .await?
        .ok_or_else(ApiError::collection_not_exists)?;

    Ok(Json(collection_json(
        &model,
        include_permissions_info,
        editor,
    )))
}

// ---------------------------------------------------------------------------
// POST /v1/collections/{collectionId}/update
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct UpdateCollectionBody {
    title: Option<String>,
    description: Option<String>,
}

fn validate_update_collection(body: &Value) -> Result<UpdateCollectionBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name_value(body)),
        ])));
    };

    let mut parsed = UpdateCollectionBody::default();

    match object.get("title") {
        None => {}
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = Some(title.clone());
        }
        Some(other) => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name_value(other),
        )),
    }

    match object.get("description") {
        None => {}
        Some(Value::String(description)) => parsed.description = Some(description.clone()),
        Some(other) => issues.push(issue_invalid_type(
            &["description"],
            "string",
            received_name_value(other),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn update_collection(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let collection_id = decode_id(&raw_collection_id)
        .map_err(|_| ApiError::invalid_entry_id_for("collectionId", &raw_collection_id))?;
    let args = validate_update_collection(&body)?;

    let tenant = state.config.tenant_id_override.clone();
    let current = fetch_collection(&state.db.primary, &tenant, collection_id)
        .await?
        .ok_or_else(ApiError::collection_not_exists)?;

    // `collection.checkPermission(Update)`.
    permissions::require_editor(&state, user.as_ref().map(|Extension(user)| user))?;

    let new_title = args.title.map(|value| value.trim().to_owned());
    let new_description = args.description.map(|value| value.trim().to_owned());

    if let Some(title) = &new_title {
        if title.to_lowercase() != current.title_lower
            && collection_title_exists(
                &state.db.primary,
                &tenant,
                current.parent_id,
                &title.to_lowercase(),
            )
            .await?
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                Some("COLLECTION_ALREADY_EXISTS"),
                "The collection already exists",
            ));
        }
    }

    let mut sets = Vec::new();
    let mut binds: Vec<Bind> = vec![Bind::I64(collection_id)];

    if let Some(title) = &new_title {
        binds.push(Bind::Str(title.clone()));
        sets.push(format!("title = ${}", binds.len()));
        binds.push(Bind::Str(title.to_lowercase()));
        sets.push(format!("title_lower = ${}", binds.len()));
    }
    if let Some(description) = new_description {
        binds.push(Bind::Str(description));
        sets.push(format!("description = ${}", binds.len()));
    }

    binds.push(Bind::Str("systemId".to_owned()));
    sets.push(format!("updated_by = ${}", binds.len()));
    sets.push("updated_at = CURRENT_TIMESTAMP".to_owned());

    let tenant_param = binds.len() + 1;
    binds.push(Bind::Str(tenant));

    let sql = format!(
        "UPDATE collections SET {} WHERE collection_id = $1 AND tenant_id = ${tenant_param} \
         RETURNING {COLLECTION_COLUMNS}",
        sets.join(", ")
    );

    let rows: Vec<CollectionRow> = fetch_all(&state.db.primary, &sql, &binds).await?;
    let model = rows
        .into_iter()
        .next()
        .ok_or_else(ApiError::collection_not_exists)?;

    Ok(Json(collection_json(&model, false, false)))
}

// ---------------------------------------------------------------------------
// POST /v1/collections/{collectionId}/move, POST /v1/move-collections
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MoveCollectionBody {
    parent_id: Option<i64>,
    title: Option<String>,
}

fn validate_move_collection(body: &Value) -> Result<MoveCollectionBody, ApiError> {
    let mut issues = Vec::new();
    let Some(object) = body.as_object() else {
        return Err(ApiError::validation(Value::Array(vec![
            issue_invalid_type(&[], "object", received_name_value(body)),
        ])));
    };

    let mut parsed = MoveCollectionBody::default();

    match object.get("parentId") {
        None => issues.push(issue_invalid_type(&["parentId"], "string", "undefined")),
        Some(Value::Null) => {}
        Some(Value::String(raw)) => match decode_id(raw) {
            Ok(id) => parsed.parent_id = Some(id),
            Err(_) => issues.push(encoded_id_issue("parentId", raw)),
        },
        Some(other) => issues.push(issue_invalid_type(
            &["parentId"],
            "string",
            received_name_value(other),
        )),
    }

    match object.get("title") {
        None => {}
        Some(Value::String(title)) => {
            issues.extend(entity_name_issues(title));
            parsed.title = Some(title.clone());
        }
        Some(other) => issues.push(issue_invalid_type(
            &["title"],
            "string",
            received_name_value(other),
        )),
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn move_collection_impl(
    state: &AppState,
    collection_id: i64,
    new_parent_id: Option<i64>,
    new_title: Option<String>,
    editor: bool,
) -> Result<CollectionRow, ApiError> {
    let tenant = state.config.tenant_id_override.clone();

    let collection = fetch_collection(&state.db.primary, &tenant, collection_id)
        .await?
        .ok_or_else(ApiError::collection_not_exists)?;

    // `collection.checkPermission(Move)` (and `CreateCollection` on the new
    // parent): both require the editor role in this deployment.
    if !editor {
        return Err(ApiError::access_service_permission_denied());
    }

    if let Some(parent_id) = new_parent_id {
        let parent = fetch_collection(&state.db.primary, &tenant, parent_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;

        let parent_parent_ids = match parent.parent_id {
            Some(grand_parent) => {
                get_parent_ids(&state.db.primary, &tenant, &[grand_parent]).await?
            }
            None => Vec::new(),
        };

        if parent_id == collection_id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                Some("COLLECTION_CIRCULAR_REFERENCE_ERROR"),
                "The collection cannot be a parent of itself",
            ));
        }

        if parent_parent_ids.contains(&collection_id) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                Some("COLLECTION_CIRCULAR_REFERENCE_ERROR"),
                format!(
                    "The new parent collection {} is a child of the current one – {}",
                    encode_id(parent_id),
                    encode_id(collection_id)
                ),
            ));
        }
    }

    let title = new_title
        .unwrap_or_else(|| collection.title.clone())
        .trim()
        .to_owned();

    if collection_title_exists(
        &state.db.primary,
        &tenant,
        new_parent_id,
        &title.to_lowercase(),
    )
    .await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            Some("COLLECTION_ALREADY_EXISTS"),
            "The collection already exists",
        ));
    }

    let sql = format!(
        "UPDATE collections SET title = $1, title_lower = $2, parent_id = $3, \
         updated_by = $4, updated_at = CURRENT_TIMESTAMP \
         WHERE collection_id = $5 AND tenant_id = $6 RETURNING {COLLECTION_COLUMNS}"
    );

    let rows: Vec<CollectionRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str(title.clone()),
            Bind::Str(title.to_lowercase()),
            Bind::OptI64(new_parent_id),
            Bind::Str("systemId".to_owned()),
            Bind::I64(collection_id),
            Bind::Str(tenant),
        ],
    )
    .await?;

    rows.into_iter()
        .next()
        .ok_or_else(ApiError::collection_not_exists)
}

async fn move_collection(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let collection_id = decode_id(&raw_collection_id)
        .map_err(|_| ApiError::invalid_entry_id_for("collectionId", &raw_collection_id))?;
    let args = validate_move_collection(&body)?;

    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let model =
        move_collection_impl(&state, collection_id, args.parent_id, args.title, editor).await?;

    Ok(Json(collection_json(&model, false, false)))
}

async fn move_collections(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let ids = validate_encoded_id_array(&body, "collectionIds")?.unwrap_or_default();

    let parent_id =
        match body.get("parentId") {
            None => {
                return Err(ApiError::validation(Value::Array(vec![
                    issue_invalid_type(&["parentId"], "string", "undefined"),
                ])))
            }
            Some(Value::Null) => None,
            Some(Value::String(raw)) => Some(decode_id(raw).map_err(|_| {
                ApiError::validation_issues(vec![encoded_id_issue("parentId", raw)])
            })?),
            Some(other) => {
                return Err(ApiError::validation(Value::Array(vec![
                    issue_invalid_type(&["parentId"], "string", received_name_value(other)),
                ])))
            }
        };

    let mut models = Vec::new();
    for collection_id in ids {
        models.push(move_collection_impl(&state, collection_id, parent_id, None, editor).await?);
    }

    let items: Vec<Value> = models
        .iter()
        .map(|model| collection_json(model, false, false))
        .collect();

    Ok(Json(json!({"collections": items})))
}

// ---------------------------------------------------------------------------
// DELETE /v1/collections/{collectionId}, DELETE /v1/delete-collections
// ---------------------------------------------------------------------------

async fn delete_collections_impl(
    state: &AppState,
    collection_ids: &[i64],
    editor: bool,
) -> Result<Vec<CollectionRow>, ApiError> {
    let tenant = state.config.tenant_id_override.clone();

    let sql = format!(
        "WITH RECURSIVE collectionChildren AS ( \
            SELECT * FROM collections \
            WHERE tenant_id = $1 AND deleted_at IS NULL AND collection_id = ANY($2) \
            UNION \
            SELECT collections.* FROM collections \
            JOIN collectionChildren \
              ON collectionChildren.collection_id = collections.parent_id \
            WHERE collections.tenant_id = $1 AND collections.deleted_at IS NULL \
         ) \
         SELECT {COLLECTION_COLUMNS} FROM collectionChildren"
    );

    let collections: Vec<CollectionRow> = fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str(tenant.clone()),
            Bind::I64List(collection_ids.to_vec()),
        ],
    )
    .await?;

    // `checkAndSetCollectionPermission(Delete)` for every collection that was
    // actually found (a missing id is not an error, exactly like the original).
    if !editor && !collections.is_empty() {
        return Err(ApiError::access_service_permission_denied());
    }

    let descendant_ids: Vec<i64> = collections
        .iter()
        .map(|collection| collection.collection_id)
        .collect();

    if descendant_ids.is_empty() {
        return Ok(Vec::new());
    }

    let workbooks: Vec<(i64, Option<bool>)> = fetch_all(
        &state.db.primary,
        "SELECT workbook_id, is_template FROM workbooks \
         WHERE collection_id = ANY($1) AND deleted_at IS NULL",
        &[Bind::I64List(descendant_ids.clone())],
    )
    .await?;

    for (_, is_template) in &workbooks {
        if is_template.unwrap_or(false) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                Some("COLLECTION_WITH_WORKBOOK_TEMPLATE_CANT_BE_DELETED"),
                "Collection with workbook template can't be deleted",
            ));
        }
    }

    let workbook_ids: Vec<i64> = workbooks.iter().map(|(id, _)| *id).collect();

    if !workbook_ids.is_empty() {
        crate::api::workbooks::delete_workbooks_internal(state, &workbook_ids).await?;
    }

    // Shared collection entries (same `markEntriesAsDeleted` upsert as above).
    #[derive(FromRow)]
    struct CollectionEntryRow {
        entry_id: i64,
        key: Option<String>,
        display_key: Option<String>,
        inner_meta: Option<Value>,
        scope: String,
        entry_type: String,
        created_by: String,
    }

    let entries: Vec<CollectionEntryRow> = fetch_all(
        &state.db.primary,
        "SELECT entry_id, key, display_key, inner_meta, scope::text AS scope, \
         type AS entry_type, created_by FROM entries \
         WHERE collection_id = ANY($1) AND is_deleted = FALSE AND tenant_id = $2",
        &[
            Bind::I64List(descendant_ids.clone()),
            Bind::Str(tenant.clone()),
        ],
    )
    .await?;

    if !entries.is_empty() {
        let trash: Vec<crate::api::workbooks::TrashEntry> = entries
            .into_iter()
            .map(|entry| crate::api::workbooks::TrashEntry {
                entry_id: entry.entry_id,
                key: entry.key,
                display_key: entry.display_key,
                inner_meta: entry.inner_meta,
                updated_by: "uid:systemId".to_owned(),
                scope: entry.scope,
                entry_type: entry.entry_type,
                created_by: entry.created_by,
            })
            .collect();

        crate::api::workbooks::mark_entries_as_deleted(&state.db.primary, &trash).await?;
    }

    let sql = format!(
        "UPDATE collections SET deleted_by = $1, deleted_at = CURRENT_TIMESTAMP \
         WHERE collection_id = ANY($2) AND deleted_at IS NULL \
         RETURNING {COLLECTION_COLUMNS}"
    );

    fetch_all(
        &state.db.primary,
        &sql,
        &[
            Bind::Str("systemId".to_owned()),
            Bind::I64List(descendant_ids),
        ],
    )
    .await
}

async fn delete_collection(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let collection_id = decode_id(&raw_collection_id)
        .map_err(|_| ApiError::invalid_entry_id_for("collectionId", &raw_collection_id))?;

    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let deleted = delete_collections_impl(&state, &[collection_id], editor).await?;
    let items: Vec<Value> = deleted
        .iter()
        .map(|model| collection_json(model, false, false))
        .collect();

    Ok(Json(json!({"collections": items})))
}

async fn delete_collections(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let ids = validate_encoded_id_array(&body, "collectionIds")?.unwrap_or_default();

    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let deleted = delete_collections_impl(&state, &ids, editor).await?;
    let items: Vec<Value> = deleted
        .iter()
        .map(|model| collection_json(model, false, false))
        .collect();

    Ok(Json(json!({"collections": items})))
}

// ---------------------------------------------------------------------------
// GET /v1/collections/{collectionId}/breadcrumbs
// ---------------------------------------------------------------------------

async fn breadcrumbs(
    State(state): State<AppState>,
    Path(raw_collection_id): Path<String>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let collection_id = decode_id(&raw_collection_id)
        .map_err(|_| ApiError::invalid_entry_id_for("collectionId", &raw_collection_id))?;

    let query = QueryParams::parse(&uri);
    let include_permissions_info = parse_flag(query.get("includePermissionsInfo"));
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));

    let mut parents = get_parents(
        &state.db.replica,
        &state.config.tenant_id_override,
        &[collection_id],
    )
    .await?;

    if parents.is_empty() {
        return Err(ApiError::collection_not_exists());
    }

    // `getCollectionBreadcrumbs` reverses the recursive CTE order (the root is
    // the last row of the CTE).
    parents.reverse();

    let mut items: Vec<Value> = parents
        .iter()
        .map(|model| collection_json(model, include_permissions_info, editor))
        .collect();
    items = permissions::filter_collections_by_permission(
        items
            .into_iter()
            .map(|value| {
                let permissions = value
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| permissions::collection_permissions(editor));
                (value, permissions)
            })
            .collect(),
    );

    Ok(Json(Value::Array(items)))
}

// ---------------------------------------------------------------------------
// POST /v1/collections-get-list-by-ids
// ---------------------------------------------------------------------------

async fn get_collections_list_by_ids(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let ids = validate_encoded_id_array(&body, "collectionIds")?.unwrap_or_default();
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));

    let sql = format!(
        "SELECT {COLLECTION_COLUMNS} FROM collections \
         WHERE deleted_at IS NULL AND tenant_id = $1 AND collection_id = ANY($2)"
    );

    let collections: Vec<CollectionRow> = fetch_all(
        &state.db.replica,
        &sql,
        &[
            Bind::Str(state.config.tenant_id_override.clone()),
            Bind::I64List(ids),
        ],
    )
    .await?;

    // `getCollectionsListByIds` filters on `CollectionPermission.Browse`; the
    // original response model (`collectionModelArray`) never carries permissions.
    let values = permissions::filter_collections_by_permission(
        collections
            .iter()
            .map(|model| {
                (
                    collection_json(model, false, editor),
                    permissions::collection_permissions(editor),
                )
            })
            .collect(),
    );

    Ok(Json(Value::Array(values)))
}

// ---------------------------------------------------------------------------
// GET /v1/root-collection-permissions
// ---------------------------------------------------------------------------

async fn root_permissions(
    State(state): State<AppState>,
    user: Option<Extension<AuthUser>>,
) -> Json<Value> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    Json(permissions::root_collection_permissions(editor))
}

// ---------------------------------------------------------------------------
// GET /v1/collection-content
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum PageArg {
    Default,
    Null,
    Value(i64),
}

#[derive(Debug)]
struct CollectionContentArgs {
    collection_id: Option<i64>,
    include_permissions_info: bool,
    filter_string: Option<String>,
    collections_page: PageArg,
    workbooks_page: PageArg,
    page_size: i64,
    order_field: &'static str,
    order_direction: &'static str,
    only_my: bool,
    mode: &'static str,
}

fn page_arg(value: Option<&str>) -> PageArg {
    match value {
        None => PageArg::Default,
        Some("null") => PageArg::Null,
        Some("") => PageArg::Default,
        Some(raw) => match js_parse_int(raw) {
            Some(value) => PageArg::Value(value as i64),
            None => PageArg::Default,
        },
    }
}

fn validate_collection_content(query: &QueryParams) -> Result<CollectionContentArgs, ApiError> {
    let mut issues = Vec::new();
    let mut parsed = CollectionContentArgs {
        collection_id: None,
        include_permissions_info: false,
        filter_string: None,
        collections_page: PageArg::Default,
        workbooks_page: PageArg::Default,
        page_size: 100,
        order_field: "title",
        order_direction: "ASC",
        only_my: false,
        mode: "all",
    };

    match query.get("collectionId") {
        Some(raw) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(encoded_id_issue("collectionId", raw)),
        },
        None => {}
    }

    parsed.include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    if let Some(raw) = query.get("filterString") {
        parsed.filter_string = Some(raw.to_owned());
    }

    parsed.collections_page = page_arg(query.get("collectionsPage"));
    parsed.workbooks_page = page_arg(query.get("workbooksPage"));

    if let Some(raw) = query.get("pageSize") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["pageSize"], "number", "undefined")),
            Some(value) => parsed.page_size = value as i64,
        }
    }

    if let Some(raw) = query.get("orderField") {
        match raw {
            "title" => parsed.order_field = "title",
            "createdAt" => parsed.order_field = "createdAt",
            "updatedAt" => parsed.order_field = "updatedAt",
            _ => issues.push(issue_invalid_enum(
                &["orderField"],
                &["title", "createdAt", "updatedAt"],
            )),
        }
    }

    if let Some(raw) = query.get("orderDirection") {
        match raw {
            "asc" => parsed.order_direction = "ASC",
            "desc" => parsed.order_direction = "DESC",
            _ => issues.push(issue_invalid_enum(&["orderDirection"], &["asc", "desc"])),
        }
    }

    parsed.only_my = parse_flag(query.get("onlyMy"));

    if let Some(raw) = query.get("mode") {
        match raw {
            "all" => parsed.mode = "all",
            "onlyCollections" => parsed.mode = "onlyCollections",
            "onlyWorkbooks" => parsed.mode = "onlyWorkbooks",
            _ => issues.push(issue_invalid_enum(
                &["mode"],
                &["all", "onlyCollections", "onlyWorkbooks"],
            )),
        }
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

async fn collection_content(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let query = QueryParams::parse(&uri);
    let args = validate_collection_content(&query)?;
    let tenant = state.config.tenant_id_override.clone();

    if let Some(collection_id) = args.collection_id {
        fetch_collection(&state.db.replica, &tenant, collection_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    }

    let mut collections = Vec::new();
    let mut collections_next_page_token: Option<String> = None;

    let load_collections = !matches!(args.collections_page, PageArg::Null)
        && (args.mode == "all" || args.mode == "onlyCollections");

    if load_collections {
        let page = match args.collections_page {
            PageArg::Value(value) => value,
            _ => 0,
        };

        let mut binds = vec![Bind::Str(tenant.clone())];
        let mut sql = format!(
            "SELECT {COLLECTION_COLUMNS} FROM collections \
             WHERE tenant_id = $1 AND deleted_at IS NULL"
        );

        if let Some(collection_id) = args.collection_id {
            binds.push(Bind::I64(collection_id));
            sql.push_str(&format!(" AND parent_id = ${}", binds.len()));
        } else {
            sql.push_str(" AND parent_id IS NULL");
        }

        if let Some(filter) = &args.filter_string {
            binds.push(Bind::Str(format!(
                "%{}%",
                escape_string_for_like(&filter.to_lowercase())
            )));
            sql.push_str(&format!(" AND title_lower LIKE ${}", binds.len()));
        }

        if args.only_my {
            binds.push(Bind::Str("systemId".to_owned()));
            sql.push_str(&format!(" AND created_by = ${}", binds.len()));
        }

        let order_column = match args.order_field {
            "title" => "sort_title",
            "createdAt" => "created_at",
            _ => "updated_at",
        };

        sql.push_str(&format!(
            " ORDER BY {order_column} {} LIMIT {} OFFSET {}",
            args.order_direction,
            args.page_size,
            args.page_size * page
        ));

        collections = fetch_all(&state.db.replica, &sql, &binds).await?;

        if page >= 0 && collections.len() as i64 == args.page_size {
            collections_next_page_token = Some((page + 1).to_string());
        }
    }

    let mut workbooks = Vec::new();
    let mut workbooks_next_page_token: Option<String> = None;

    let load_workbooks = !matches!(args.workbooks_page, PageArg::Null)
        && (args.mode == "all" || args.mode == "onlyWorkbooks");

    if load_workbooks {
        let page = match args.workbooks_page {
            PageArg::Value(value) => value,
            _ => 0,
        };

        let list_args = crate::api::workbooks::WorkbookListArgs {
            collection_id: args.collection_id,
            include_permissions_info: args.include_permissions_info,
            filter_string: args.filter_string.clone(),
            order_field: args.order_field,
            order_direction: args.order_direction,
            only_my: args.only_my,
            page,
            page_size: args.page_size,
        };

        let (rows, token) = crate::api::workbooks::query_workbooks(&state, &list_args).await?;
        workbooks = rows;
        workbooks_next_page_token = token;
    }

    // `getCollectionContent` drops collections the caller cannot `browse`.
    let collections_json: Vec<Value> = if permissions::collection_browsable(editor) {
        collections
            .iter()
            .map(|model| collection_json(model, args.include_permissions_info, editor))
            .collect()
    } else {
        Vec::new()
    };
    let workbooks_json: Vec<Value> = workbooks
        .iter()
        .map(|model| {
            crate::api::workbooks::workbook_json(model, args.include_permissions_info, editor)
        })
        .collect();

    Ok(Json(json!({
        "collections": collections_json,
        "workbooks": workbooks_json,
        "collectionsNextPageToken": collections_next_page_token,
        "workbooksNextPageToken": workbooks_next_page_token,
    })))
}

// ---------------------------------------------------------------------------
// GET /v1/structure-items
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StructureItemsArgs {
    collection_id: Option<i64>,
    include_permissions_info: bool,
    filter_string: Option<String>,
    page: Option<String>,
    page_size: i64,
    order_field: &'static str,
    order_direction: &'static str,
    only_my: bool,
    mode: &'static str,
}

fn validate_structure_items(query: &QueryParams) -> Result<StructureItemsArgs, ApiError> {
    let mut issues = Vec::new();
    let mut parsed = StructureItemsArgs {
        collection_id: None,
        include_permissions_info: false,
        filter_string: None,
        page: None,
        page_size: 100,
        order_field: "title",
        order_direction: "ASC",
        only_my: false,
        mode: "all",
    };

    match query.get("collectionId") {
        Some(raw) => match decode_id(raw) {
            Ok(id) => parsed.collection_id = Some(id),
            Err(_) => issues.push(encoded_id_issue("collectionId", raw)),
        },
        None => {}
    }

    parsed.include_permissions_info = parse_flag(query.get("includePermissionsInfo"));

    if let Some(raw) = query.get("filterString") {
        parsed.filter_string = Some(raw.to_owned());
    }

    parsed.page = query.get("page").map(ToOwned::to_owned);

    if let Some(raw) = query.get("pageSize") {
        match js_parse_int(raw) {
            None => issues.push(issue_invalid_type(&["pageSize"], "number", "undefined")),
            Some(value) if value < 1.0 => issues.push(issue_too_small_number(&["pageSize"], 1)),
            Some(value) if value > 200.0 => issues.push(issue_too_big_number(&["pageSize"], 200)),
            Some(value) => parsed.page_size = value as i64,
        }
    }

    if let Some(raw) = query.get("orderField") {
        match raw {
            "title" => parsed.order_field = "title",
            "createdAt" => parsed.order_field = "createdAt",
            "updatedAt" => parsed.order_field = "updatedAt",
            _ => issues.push(issue_invalid_enum(
                &["orderField"],
                &["title", "createdAt", "updatedAt"],
            )),
        }
    }

    if let Some(raw) = query.get("orderDirection") {
        match raw {
            "asc" => parsed.order_direction = "ASC",
            "desc" => parsed.order_direction = "DESC",
            _ => issues.push(issue_invalid_enum(&["orderDirection"], &["asc", "desc"])),
        }
    }

    parsed.only_my = parse_flag(query.get("onlyMy"));

    if let Some(raw) = query.get("mode") {
        match raw {
            "all" => parsed.mode = "all",
            "onlyCollections" => parsed.mode = "onlyCollections",
            "onlyWorkbooks" => parsed.mode = "onlyWorkbooks",
            "onlyEntries" => parsed.mode = "onlyEntries",
            _ => issues.push(issue_invalid_enum(
                &["mode"],
                &["all", "onlyCollections", "onlyWorkbooks", "onlyEntries"],
            )),
        }
    }

    if issues.is_empty() && parsed.mode == "onlyEntries" && parsed.collection_id.is_none() {
        issues.push(issue_custom(
            &["mode", "collectionId"],
            "'collectionId' is required if mode is 'onlyEntries'",
        ));
    }

    if issues.is_empty() {
        Ok(parsed)
    } else {
        Err(ApiError::validation_issues(issues))
    }
}

#[derive(Debug, FromRow)]
struct StructureRow {
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    title: Option<String>,
    description: Option<String>,
    parent_id: Option<i64>,
    tenant_id: Option<String>,
    created_by: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_by: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    meta: Option<Value>,
    status: Option<String>,
    entry_id: Option<i64>,
    scope: Option<String>,
    entry_type: Option<String>,
    display_key: Option<String>,
    key: Option<String>,
    entity_id: i64,
    cursor0: String,
    cursor1: Option<String>,
    cursor2: Option<String>,
}

async fn structure_items(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    user: Option<Extension<AuthUser>>,
) -> Result<Json<Value>, ApiError> {
    let editor = permissions::is_editor_for(&state, user.as_ref().map(|Extension(user)| user));
    let query = QueryParams::parse(&uri);
    let args = validate_structure_items(&query)?;
    let tenant = state.config.tenant_id_override.clone();

    if let Some(collection_id) = args.collection_id {
        fetch_collection(&state.db.replica, &tenant, collection_id)
            .await?
            .ok_or_else(ApiError::collection_not_exists)?;
    } else if args.mode == "onlyEntries" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            Some("ENTRIES_REQUIRE_COLLECTION_ID"),
            "Collection ID is required for entries",
        ));
    }

    let mut binds: Vec<Bind> = vec![Bind::Str(tenant.clone())];

    let (use_collections, use_workbooks, use_entries) = match args.mode {
        "onlyCollections" => (true, false, false),
        "onlyWorkbooks" => (false, true, false),
        "onlyEntries" => (false, false, true),
        _ => (true, true, args.collection_id.is_some()),
    };

    let mut parts: Vec<String> = Vec::new();

    if use_collections {
        let mut sql = String::from(
            "SELECT 1 AS rank, NULL::bigint AS workbook_id, collection_id, title, sort_title, \
             description, parent_id, tenant_id, created_by, created_at, updated_by, updated_at, \
             meta, NULL::text AS status, NULL::bigint AS entry_id, NULL::text AS scope, \
             NULL::text AS entry_type, NULL::text AS display_key, NULL::text AS \"key\", \
             collection_id AS entity_id FROM collections \
             WHERE tenant_id = $1 AND deleted_at IS NULL",
        );

        if let Some(collection_id) = args.collection_id {
            binds.push(Bind::I64(collection_id));
            sql.push_str(&format!(" AND parent_id = ${}", binds.len()));
        } else {
            sql.push_str(" AND parent_id IS NULL");
        }

        if let Some(filter) = &args.filter_string {
            binds.push(Bind::Str(format!(
                "%{}%",
                escape_string_for_like(&filter.to_lowercase())
            )));
            sql.push_str(&format!(" AND title_lower LIKE ${}", binds.len()));
        }

        if args.only_my {
            binds.push(Bind::Str("systemId".to_owned()));
            sql.push_str(&format!(" AND created_by = ${}", binds.len()));
        }

        parts.push(sql);
    }

    if use_workbooks {
        let mut sql = String::from(
            "SELECT 2 AS rank, workbook_id, collection_id, title, sort_title, description, \
             NULL::bigint AS parent_id, tenant_id, created_by, created_at, updated_by, updated_at, \
             meta, status::text AS status, NULL::bigint AS entry_id, NULL::text AS scope, \
             NULL::text AS entry_type, NULL::text AS display_key, NULL::text AS \"key\", \
             workbook_id AS entity_id FROM workbooks \
             WHERE tenant_id = $1 AND deleted_at IS NULL",
        );

        if let Some(collection_id) = args.collection_id {
            binds.push(Bind::I64(collection_id));
            sql.push_str(&format!(" AND collection_id = ${}", binds.len()));
        } else {
            sql.push_str(" AND collection_id IS NULL");
        }

        if let Some(filter) = &args.filter_string {
            binds.push(Bind::Str(format!(
                "%{}%",
                escape_string_for_like(&filter.to_lowercase())
            )));
            sql.push_str(&format!(" AND title_lower LIKE ${}", binds.len()));
        }

        if args.only_my {
            binds.push(Bind::Str("systemId".to_owned()));
            sql.push_str(&format!(" AND created_by = ${}", binds.len()));
        }

        parts.push(sql);
    }

    if use_entries {
        let mut sql = String::from(
            "SELECT 3 AS rank, workbook_id, collection_id, NULL::text AS title, \
             sort_name AS sort_title, NULL::text AS description, NULL::bigint AS parent_id, \
             tenant_id, created_by, created_at, updated_by, updated_at, NULL::jsonb AS meta, \
             NULL::text AS status, entry_id, scope::text AS scope, type AS entry_type, \
             display_key, \"key\", entry_id AS entity_id FROM entries \
             WHERE is_deleted = FALSE",
        );

        let collection_id = args.collection_id.unwrap_or_default();
        binds.push(Bind::I64(collection_id));
        sql.push_str(&format!(" AND collection_id = ${}", binds.len()));
        binds.push(Bind::Str(tenant.clone()));
        sql.push_str(&format!(" AND tenant_id = ${}", binds.len()));

        if let Some(filter) = &args.filter_string {
            binds.push(Bind::Str(format!(
                "%{}%",
                escape_string_for_like(&filter.to_lowercase())
            )));
            sql.push_str(&format!(" AND name LIKE ${}", binds.len()));
        }

        if args.only_my {
            binds.push(Bind::Str("uid:systemId".to_owned()));
            sql.push_str(&format!(" AND created_by = ${}", binds.len()));
        }

        parts.push(sql);
    }

    let union_sql = parts.join(" UNION ALL ");

    let sort_column = match args.order_field {
        "title" => "sort_title",
        "createdAt" => "created_at",
        _ => "updated_at",
    };
    let sort_cast = if args.order_field == "title" {
        "bytea"
    } else {
        "timestamptz"
    };

    let ascending = args.order_direction == "ASC";

    let (cursor_fields, cursor_select, order_by) = if args.mode == "all" {
        let fields = vec![
            CursorField {
                ident: "u.rank",
                bind_cast: "integer",
                asc: true,
                validator: TokenValidator::AnyString,
            },
            CursorField {
                ident: if sort_column == "sort_title" {
                    "u.sort_title"
                } else if sort_column == "created_at" {
                    "u.created_at"
                } else {
                    "u.updated_at"
                },
                bind_cast: sort_cast,
                asc: ascending,
                validator: if args.order_field == "title" {
                    TokenValidator::AnyString
                } else {
                    TokenValidator::Timestampz
                },
            },
            CursorField {
                ident: "u.entity_id",
                bind_cast: "bigint",
                asc: ascending,
                validator: TokenValidator::BigInt,
            },
        ];

        (
            fields,
            format!(
                "(u.rank)::text AS cursor0, (u.{sort_column})::text AS cursor1, \
                 (u.entity_id)::text AS cursor2"
            ),
            format!(
                "u.rank ASC, u.{sort_column} {}, u.entity_id {}",
                args.order_direction, args.order_direction
            ),
        )
    } else {
        let fields = vec![
            CursorField {
                ident: if sort_column == "sort_title" {
                    "u.sort_title"
                } else if sort_column == "created_at" {
                    "u.created_at"
                } else {
                    "u.updated_at"
                },
                bind_cast: sort_cast,
                asc: ascending,
                validator: if args.order_field == "title" {
                    TokenValidator::AnyString
                } else {
                    TokenValidator::Timestampz
                },
            },
            CursorField {
                ident: "u.entity_id",
                bind_cast: "bigint",
                asc: ascending,
                validator: TokenValidator::BigInt,
            },
        ];

        (
            fields,
            format!(
                "(u.{sort_column})::text AS cursor0, (u.entity_id)::text AS cursor1, \
                 NULL::text AS cursor2"
            ),
            format!(
                "u.{sort_column} {}, u.entity_id {}",
                args.order_direction, args.order_direction
            ),
        )
    };

    let mut sql = format!("SELECT *, {cursor_select} FROM ({union_sql}) AS u");

    if let Some(token) = args.page.as_deref().filter(|token| !token.is_empty()) {
        let values = decode_page_token(token, &cursor_fields)?;
        let (clause, cursor_binds) = build_cursor_clause(&cursor_fields, &values, binds.len() + 1);
        sql.push_str(&format!(" WHERE {clause}"));
        binds.extend(cursor_binds.into_iter().map(Bind::Str));
    }

    sql.push_str(&format!(
        " ORDER BY {order_by} LIMIT {}",
        args.page_size + 1
    ));

    let rows: Vec<StructureRow> = fetch_all(&state.db.replica, &sql, &binds).await?;

    let has_more = rows.len() as i64 > args.page_size;
    let visible = if has_more {
        args.page_size as usize
    } else {
        rows.len()
    };

    let mut items = Vec::with_capacity(visible);

    for row in &rows[..visible] {
        if row.entry_id.is_some() {
            // `createCollectionEntry` throws from the collection-entry
            // constructor in the open-source registry plugin.
            return Err(ApiError::collection_entry_unsupported_scope());
        }

        let include_permissions = args.include_permissions_info;

        if let Some(workbook_id) = row.workbook_id {
            let mut value = crate::api::workbooks::workbook_json_parts(
                workbook_id,
                row.collection_id,
                row.title.as_deref().unwrap_or_default(),
                row.description.as_deref(),
                row.tenant_id.as_deref().unwrap_or_default(),
                row.meta.as_ref().unwrap_or(&Value::Null),
                row.created_by.as_deref().unwrap_or_default(),
                row.created_at.unwrap_or_else(Utc::now),
                row.updated_by.as_deref(),
                row.updated_at.unwrap_or_else(Utc::now),
                row.status.as_deref().unwrap_or("active"),
                include_permissions,
                editor,
            );
            if let Value::Object(map) = &mut value {
                map.insert("entity".into(), json!("workbook"));
            }
            items.push(value);
        } else {
            if !permissions::collection_browsable(editor) {
                continue;
            }
            let mut value = collection_json_parts(
                row.entity_id,
                row.title.as_deref().unwrap_or_default(),
                row.description.as_deref(),
                row.parent_id,
                row.tenant_id.as_deref().unwrap_or_default(),
                row.created_by.as_deref().unwrap_or_default(),
                row.created_at.unwrap_or_else(Utc::now),
                row.updated_by.as_deref().unwrap_or_default(),
                row.updated_at.unwrap_or_else(Utc::now),
                row.meta.as_ref().unwrap_or(&Value::Null),
                include_permissions,
                editor,
            );
            if let Value::Object(map) = &mut value {
                map.insert("entity".into(), json!("collection"));
            }
            items.push(value);
        }
    }

    let next_page_token = if has_more {
        let last = &rows[visible - 1];
        let values: Vec<String> = if args.mode == "all" {
            vec![
                last.cursor0.clone(),
                last.cursor1.clone().unwrap_or_default(),
                last.cursor2.clone().unwrap_or_default(),
            ]
        } else {
            vec![
                last.cursor0.clone(),
                last.cursor1.clone().unwrap_or_default(),
            ]
        };
        Some(encode_page_token(&values))
    } else {
        None
    };

    Ok(Json(json!({
        "items": items,
        "nextPageToken": next_page_token,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_permissions_shape() {
        // `collectionPermissionsSchema` of the original response model lists 13
        // of the 16 `getAllPermissions` keys; the extra three are still sent.
        let value = permissions::full_collection_permissions();
        for key in [
            "listAccessBindings",
            "updateAccessBindings",
            "createCollection",
            "createWorkbook",
            "createSharedEntry",
            "createSparkCluster",
            "createTrinoCluster",
            "limitedView",
            "view",
            "update",
            "copy",
            "move",
            "delete",
            "securityManage",
            "securityApprove",
            "browse",
        ] {
            assert_eq!(value[key], json!(true), "{key}");
        }
        assert_eq!(value.as_object().map(|map| map.len()), Some(16));
    }

    #[test]
    fn page_args_mimic_controller_parsing() {
        assert!(matches!(page_arg(None), PageArg::Default));
        assert!(matches!(page_arg(Some("")), PageArg::Default));
        assert!(matches!(page_arg(Some("null")), PageArg::Null));
        assert!(matches!(page_arg(Some("2")), PageArg::Value(2)));
    }
}
