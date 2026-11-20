//! User and user-management routes: the public `/v1/users/*`, `/v1/management/users/*`
//! surface and their `/private/v1/...` master-token variants.
//!
//! Guard order mirrors the Node middleware chain: body-parser (malformed JSON → 400) →
//! Bearer auth / private master token → permission(`Manage`/`InstanceUse`) → route checks
//! (`ManageLocalUsers`) → controller zod validation → service.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::db;
use crate::guards;
use crate::http::{self, ParsedBody};
use crate::models::{
    CreateUserResponse, ProfileResponse, UserOut, UsersByIdsResponse, UsersListResponse,
};
use crate::password;
use crate::validation::{self, DeltasError, PathIdError, SubjectIdsError};
use crate::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/users/list", get(users_list))
        .route("/v1/users/get-by-ids", post(users_by_ids))
        .route("/v1/users/me/profile", post(update_my_profile))
        .route("/v1/users/me/password", post(update_my_password))
        .route("/v1/management/users/create", post(management_create_user))
        .route("/v1/management/users/roles/add", post(management_roles_add))
        .route("/v1/management/users/roles/update", post(management_roles_update))
        .route("/v1/management/users/roles/remove", post(management_roles_remove))
        .route(
            "/v1/management/users/{user_id}/profile",
            get(management_get_profile).post(management_update_profile),
        )
        .route(
            "/v1/management/users/{user_id}/password",
            post(management_update_password),
        )
        .route("/v1/management/users/{user_id}", delete(management_delete_user))
        // Master-token variants: the reference routes exist only for create/delete/profile/
        // password/roles (no private list or get-profile).
        .route(
            "/private/v1/management/users/create",
            post(private_create_user),
        )
        .route(
            "/private/v1/management/users/roles/add",
            post(private_roles_add),
        )
        .route(
            "/private/v1/management/users/roles/update",
            post(private_roles_update),
        )
        .route(
            "/private/v1/management/users/roles/remove",
            post(private_roles_remove),
        )
        .route(
            "/private/v1/management/users/{user_id}/profile",
            post(private_update_profile),
        )
        .route(
            "/private/v1/management/users/{user_id}/password",
            post(private_update_password),
        )
        .route(
            "/private/v1/management/users/{user_id}",
            delete(private_delete_user),
        )
}

// ---------------------------------------------------------------------------------------
// error envelopes
// ---------------------------------------------------------------------------------------

fn done(request_id: &str) -> Response {
    http::json_value(StatusCode::OK, json!({"done": true}), request_id)
}

fn user_not_exists(request_id: &str) -> Response {
    http::json_value(
        StatusCode::NOT_FOUND,
        json!({"code": "AUTH.USER_NOT_EXISTS", "message": "The user doesn't exist"}),
        request_id,
    )
}

fn idp_change_not_allowed(request_id: &str) -> Response {
    http::json_value(
        StatusCode::FORBIDDEN,
        json!({
            "code": "AUTH.IDP_USER_CHANGE_NOT_ALLOWED",
            "message": "Not allowed to change IdP user settings",
        }),
        request_id,
    )
}

fn old_password_incorrect(request_id: &str) -> Response {
    http::json_value(
        StatusCode::BAD_REQUEST,
        json!({
            "code": "AUTH.OLD_PASSWORD_INCORRECT",
            "message": "The old password is incorrect",
        }),
        request_id,
    )
}

fn user_already_exists(request_id: &str) -> Response {
    http::json_value(
        StatusCode::CONFLICT,
        json!({"code": "AUTH.USER_ALREADY_EXISTS", "message": "The user already exists"}),
        request_id,
    )
}

fn db_unique_violation(request_id: &str) -> Response {
    http::json_value(
        StatusCode::BAD_REQUEST,
        json!({"code": "AUTH.DB.UNIQUE_VIOLATION", "message": "The entity already exists"}),
        request_id,
    )
}

fn numeric_value_out_of_range(request_id: &str) -> Response {
    http::json_value(
        StatusCode::BAD_REQUEST,
        json!({"code": "AUTH.DB.NUMERIC_VALUE_OUT_OF_RANGE", "message": "Wrong passed id"}),
        request_id,
    )
}

fn role_not_exists(message: &str, request_id: &str) -> Response {
    http::json_value(
        StatusCode::NOT_FOUND,
        json!({"code": "AUTH.ROLE_NOT_EXISTS", "message": message}),
        request_id,
    )
}

fn not_consistent(message: &str, request_id: &str) -> Response {
    http::json_value(
        StatusCode::BAD_REQUEST,
        json!({"code": "AUTH.NOT_CONSISTENT", "message": message}),
        request_id,
    )
}

// ---------------------------------------------------------------------------------------
// request helpers
// ---------------------------------------------------------------------------------------

/// express body-parser runs before every auth/permission middleware.
fn body_value(headers: &HeaderMap, body: &Bytes, request_id: &str) -> Result<Option<Value>, Response> {
    match http::parse_json_body(headers, body) {
        ParsedBody::Malformed => Err(http::malformed_json(request_id)),
        ParsedBody::NotJson => Ok(None),
        ParsedBody::Value(value) => Ok(Some(value)),
    }
}

async fn authenticated(state: &AppState, headers: &HeaderMap, request_id: &str) -> Result<guards::AuthUser, Response> {
    guards::auth_user(state, headers, request_id)
}

/// `Manage` permission + (optionally) the `ManageLocalUsers` route check, with the DB client
/// the permission introspection needs.
async fn management_client(
    state: &AppState,
    headers: &HeaderMap,
    request_id: &str,
    check_manage_local_users: bool,
) -> Result<tokio_postgres::Client, Response> {
    let auth = authenticated(state, headers, request_id).await?;
    let client = state
        .db
        .acquire()
        .await
        .map_err(|_| http::database_error(request_id))?;
    if let Err(response) = guards::manage_permission(&client, &auth, request_id).await {
        state.db.release(client).await;
        return Err(response);
    }
    if check_manage_local_users {
        if let Err(response) = guards::manage_local_users(&state.config, request_id) {
            state.db.release(client).await;
            return Err(response);
        }
    }
    Ok(client)
}

/// `/private/*`: the master token gate runs before the controller, so before zod validation.
fn private_guard(
    state: &AppState,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<(), Response> {
    guards::master_token(&state.config, headers, request_id)
}

fn decode_path(raw: &str, request_id: &str) -> Result<i64, Response> {
    match validation::decode_path_id(raw) {
        Ok(id) => Ok(id),
        Err(PathIdError::Validation(issues)) => Err(http::validation_error(request_id, issues)),
        Err(PathIdError::NumericOverflow) => Err(numeric_value_out_of_range(request_id)),
    }
}

fn check_local_user_row(
    row: Option<(Option<String>, Option<String>)>,
    is_private: bool,
    request_id: &str,
) -> Result<Option<String>, Response> {
    match row {
        None => Err(user_not_exists(request_id)),
        Some((password, idp_slug)) => {
            if !is_private && idp_slug.is_some() {
                return Err(idp_change_not_allowed(request_id));
            }
            Ok(password)
        }
    }
}

// ---------------------------------------------------------------------------------------
// GET /v1/users/list
// ---------------------------------------------------------------------------------------

async fn users_list(
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let request_id = http::request_id(&headers);
    let auth = match authenticated(&state, &headers, &request_id).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if let Err(response) = guards::instance_use(&auth, &request_id) {
        return response;
    }
    let query = match validation::validate_list_query(raw.as_deref()) {
        Ok(query) => query,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let result = db::list_users(&client, &query).await;
    state.db.release(client).await;
    match result {
        Ok((users, next_page_token)) => http::json_body(
            StatusCode::OK,
            &UsersListResponse {
                next_page_token,
                users: users.iter().map(UserOut::from_data).collect(),
            },
            &request_id,
        ),
        Err(error) => {
            tracing::warn!(%error, "list users failed");
            http::database_error(&request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// POST /v1/users/get-by-ids
// ---------------------------------------------------------------------------------------

async fn users_by_ids(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let auth = match authenticated(&state, &headers, &request_id).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if let Err(response) = guards::instance_use(&auth, &request_id) {
        return response;
    }
    let ids = match validation::validate_subject_ids(body.as_ref()) {
        Ok(ids) => ids,
        Err(SubjectIdsError::Validation(issues)) => {
            return http::validation_error(&request_id, issues)
        }
        Err(SubjectIdsError::NumericOverflow) => return numeric_value_out_of_range(&request_id),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let result = db::users_by_ids(&client, &ids).await;
    state.db.release(client).await;
    match result {
        Ok(users) => http::json_body(
            StatusCode::OK,
            &UsersByIdsResponse {
                users: users.iter().map(UserOut::from_data).collect(),
            },
            &request_id,
        ),
        Err(error) => {
            tracing::warn!(%error, "users by ids failed");
            http::database_error(&request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// POST /v1/users/me/profile and /v1/users/me/password
// ---------------------------------------------------------------------------------------

async fn update_my_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let auth = match authenticated(&state, &headers, &request_id).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if let Err(response) = guards::manage_local_users(&state.config, &request_id) {
        return response;
    }
    let input = match validation::validate_profile_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let result = db::patch_profile(&client, auth.user_id, &input).await;
    state.db.release(client).await;
    match result {
        Ok(()) => done(&request_id),
        Err(error) => {
            tracing::warn!(%error, "update my profile failed");
            http::database_error(&request_id)
        }
    }
}

async fn update_my_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let auth = match authenticated(&state, &headers, &request_id).await {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    if let Err(response) = guards::manage_local_users(&state.config, &request_id) {
        return response;
    }
    let input = match validation::validate_me_password_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let result = update_password_with_check(
        &client,
        auth.user_id,
        input.old_password,
        input.new_password,
        false,
        true,
        &request_id,
    )
    .await;
    state.db.release(client).await;
    result
}

/// Shared by the public/private management password routes and `/v1/users/me/password`.
#[allow(clippy::too_many_arguments)]
async fn update_password_with_check(
    client: &tokio_postgres::Client,
    user_id: i64,
    old_password: String,
    new_password: String,
    is_private: bool,
    check_old_password: bool,
    request_id: &str,
) -> Response {
    let row = match db::user_password_row(client, user_id).await {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%error, "read password row failed");
            return http::database_error(request_id);
        }
    };
    let stored = match check_local_user_row(row, is_private, request_id) {
        Ok(stored) => stored,
        Err(response) => return response,
    };
    if check_old_password {
        let stored = stored.unwrap_or_default();
        let verified = tokio::task::spawn_blocking(move || {
            !stored.is_empty() && password::verify_password(&stored, &old_password)
        })
        .await
        .unwrap_or(false);
        if !verified {
            return old_password_incorrect(request_id);
        }
    }
    let Ok(Some(password_hash)) =
        tokio::task::spawn_blocking(move || password::hash_password(&new_password)).await
    else {
        return http::internal_error(request_id);
    };
    match db::set_password(client, user_id, &password_hash).await {
        Ok(()) => done(request_id),
        Err(error) => {
            tracing::warn!(%error, "set password failed");
            http::database_error(request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// GET /v1/management/users/:userId/profile
// ---------------------------------------------------------------------------------------

async fn management_get_profile(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = http::request_id(&headers);
    let client = match management_client(&state, &headers, &request_id, false).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => {
            state.db.release(client).await;
            return response;
        }
    };
    let result = db::get_profile(&client, user_id).await;
    state.db.release(client).await;
    match result {
        Ok(Some(user)) => http::json_body(
            StatusCode::OK,
            &ProfileResponse {
                profile: UserOut::from_data(&user),
            },
            &request_id,
        ),
        Ok(None) => user_not_exists(&request_id),
        Err(error) => {
            tracing::warn!(%error, "get user profile failed");
            http::database_error(&request_id)
        }
    }
}

/// Shared profile update (public management route and its private variant).
async fn update_profile_inner(
    client: &tokio_postgres::Client,
    user_id: i64,
    input: validation::ProfileInput,
    is_private: bool,
    request_id: &str,
) -> Response {
    let idp_slug = match db::user_idp_slug(client, user_id).await {
        Ok(slug) => slug,
        Err(error) => {
            tracing::warn!(%error, "read user idp slug failed");
            return http::database_error(request_id);
        }
    };
    let Some(idp_slug) = idp_slug else {
        return user_not_exists(request_id);
    };
    if !is_private && idp_slug.is_some() {
        return idp_change_not_allowed(request_id);
    }
    match db::patch_profile(client, user_id, &input).await {
        Ok(()) => done(request_id),
        Err(error) => {
            tracing::warn!(%error, "update profile failed");
            http::database_error(request_id)
        }
    }
}

async fn management_update_profile(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => {
            state.db.release(client).await;
            return response;
        }
    };
    let input = match validation::validate_profile_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => {
            state.db.release(client).await;
            return http::validation_error(&request_id, issues);
        }
    };
    let response = update_profile_inner(&client, user_id, input, false, &request_id).await;
    state.db.release(client).await;
    response
}

async fn management_update_password(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => {
            state.db.release(client).await;
            return response;
        }
    };
    let input = match validation::validate_admin_password_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => {
            state.db.release(client).await;
            return http::validation_error(&request_id, issues);
        }
    };
    // The management route replaces the password without knowing the old one.
    let stored = match db::user_password_row(&client, user_id).await {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%error, "read password row failed");
            state.db.release(client).await;
            return http::database_error(&request_id);
        }
    };
    if let Err(response) = check_local_user_row(stored, false, &request_id) {
        state.db.release(client).await;
        return response;
    }
    let Ok(Some(password_hash)) =
        tokio::task::spawn_blocking(move || password::hash_password(&input.new_password)).await
    else {
        state.db.release(client).await;
        return http::internal_error(&request_id);
    };
    let result = db::set_password(&client, user_id, &password_hash).await;
    state.db.release(client).await;
    match result {
        Ok(()) => done(&request_id),
        Err(error) => {
            tracing::warn!(%error, "set password failed");
            http::database_error(&request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// POST /v1/management/users/create
// ---------------------------------------------------------------------------------------

async fn create_user_inner(
    state: &AppState,
    client: &mut tokio_postgres::Client,
    input: validation::CreateUserInput,
    request_id: &str,
) -> Response {
    let password_for_hash = input.password.clone();
    let Ok(Some(password_hash)) =
        tokio::task::spawn_blocking(move || password::hash_password(&password_for_hash)).await
    else {
        return http::internal_error(request_id);
    };
    let roles = input
        .roles
        .unwrap_or_else(|| vec![state.config.default_role.clone()]);
    let result = db::create_user(
        client,
        &input.login,
        &password_hash,
        input.email.as_deref(),
        input.first_name.as_deref(),
        input.last_name.as_deref(),
        &roles,
    )
    .await;
    match result {
        Ok(user_id) => http::json_body(
            StatusCode::OK,
            &CreateUserResponse {
                user_id: crate::ids::encode_id(user_id),
            },
            request_id,
        ),
        Err(db::CreateUserError::Exists) => user_already_exists(request_id),
        Err(db::CreateUserError::UniqueViolation) => db_unique_violation(request_id),
        Err(db::CreateUserError::Db(error)) => {
            tracing::warn!(%error, "create user failed");
            http::database_error(request_id)
        }
    }
}

async fn management_create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let input = match validation::validate_create_user_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => {
            state.db.release(client).await;
            return http::validation_error(&request_id, issues);
        }
    };
    let response = create_user_inner(&state, &mut client, input, &request_id).await;
    state.db.release(client).await;
    response
}

// ---------------------------------------------------------------------------------------
// DELETE /v1/management/users/:userId
// ---------------------------------------------------------------------------------------

async fn delete_user_inner(
    client: &tokio_postgres::Client,
    user_id: i64,
    is_private: bool,
    request_id: &str,
) -> Response {
    let idp_slug = match db::user_idp_slug(client, user_id).await {
        Ok(slug) => slug,
        Err(error) => {
            tracing::warn!(%error, "read user idp slug failed");
            return http::database_error(request_id);
        }
    };
    let Some(idp_slug) = idp_slug else {
        return user_not_exists(request_id);
    };
    if !is_private && idp_slug.is_some() {
        return idp_change_not_allowed(request_id);
    }
    match db::delete_user(client, user_id).await {
        Ok(()) => done(request_id),
        Err(error) => {
            tracing::warn!(%error, "delete user failed");
            http::database_error(request_id)
        }
    }
}

async fn management_delete_user(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = http::request_id(&headers);
    let client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => {
            state.db.release(client).await;
            return response;
        }
    };
    let response = delete_user_inner(&client, user_id, false, &request_id).await;
    state.db.release(client).await;
    response
}

// ---------------------------------------------------------------------------------------
// roles add / update / remove
// ---------------------------------------------------------------------------------------

async fn roles_add_inner(
    client: &mut tokio_postgres::Client,
    deltas: Vec<(String, i64)>,
    request_id: &str,
) -> Response {
    match db::add_roles(client, &deltas).await {
        Ok(()) => done(request_id),
        Err(error) => {
            tracing::warn!(%error, "add roles failed");
            http::database_error(request_id)
        }
    }
}

async fn roles_update_inner(
    client: &mut tokio_postgres::Client,
    deltas: Vec<(String, String, i64)>,
    request_id: &str,
) -> Response {
    match db::update_roles(client, &deltas).await {
        Ok(()) => done(request_id),
        Err(db::RoleUpdateError::NotExists(message)) => role_not_exists(&message, request_id),
        Err(db::RoleUpdateError::NotConsistent(message)) => not_consistent(&message, request_id),
        Err(db::RoleUpdateError::Db(error)) => {
            tracing::warn!(%error, "update roles failed");
            http::database_error(request_id)
        }
    }
}

async fn roles_remove_inner(
    client: &mut tokio_postgres::Client,
    deltas: Vec<(String, i64)>,
    request_id: &str,
) -> Response {
    match db::remove_roles(client, &deltas).await {
        Ok(()) => done(request_id),
        Err(error) => {
            tracing::warn!(%error, "remove roles failed");
            http::database_error(request_id)
        }
    }
}

fn deltas_error(response: DeltasError, request_id: &str) -> Response {
    match response {
        DeltasError::Validation(issues) => http::validation_error(request_id, issues),
        DeltasError::NumericOverflow => numeric_value_out_of_range(request_id),
    }
}

async fn management_roles_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let deltas = match validation::validate_add_remove_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => {
            state.db.release(client).await;
            return deltas_error(error, &request_id);
        }
    };
    let response = roles_add_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}

async fn management_roles_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let deltas = match validation::validate_update_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => {
            state.db.release(client).await;
            return deltas_error(error, &request_id);
        }
    };
    let response = roles_update_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}

async fn management_roles_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut client = match management_client(&state, &headers, &request_id, true).await {
        Ok(client) => client,
        Err(response) => return response,
    };
    let deltas = match validation::validate_add_remove_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => {
            state.db.release(client).await;
            return deltas_error(error, &request_id);
        }
    };
    let response = roles_remove_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}

// ---------------------------------------------------------------------------------------
// /private/v1/... master-token variants
// ---------------------------------------------------------------------------------------

async fn private_create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let input = match validation::validate_create_user_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(mut client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = create_user_inner(&state, &mut client, input, &request_id).await;
    state.db.release(client).await;
    response
}

async fn private_update_profile(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let input = match validation::validate_profile_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = update_profile_inner(&client, user_id, input, true, &request_id).await;
    state.db.release(client).await;
    response
}

async fn private_update_password(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let input = match validation::validate_admin_password_body(body.as_ref()) {
        Ok(input) => input,
        Err(issues) => return http::validation_error(&request_id, issues),
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = update_password_with_check(
        &client,
        user_id,
        String::new(),
        input.new_password,
        true,
        false,
        &request_id,
    )
    .await;
    state.db.release(client).await;
    response
}

async fn private_delete_user(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let request_id = http::request_id(&headers);
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let user_id = match decode_path(&user_id, &request_id) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let Ok(client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = delete_user_inner(&client, user_id, true, &request_id).await;
    state.db.release(client).await;
    response
}

async fn private_roles_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let deltas = match validation::validate_add_remove_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => return deltas_error(error, &request_id),
    };
    let Ok(mut client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = roles_add_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}

async fn private_roles_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let deltas = match validation::validate_update_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => return deltas_error(error, &request_id),
    };
    let Ok(mut client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = roles_update_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}

async fn private_roles_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let body = match body_value(&headers, &body, &request_id) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = private_guard(&state, &headers, &request_id) {
        return response;
    }
    let deltas = match validation::validate_add_remove_deltas(body.as_ref()) {
        Ok(deltas) => deltas,
        Err(error) => return deltas_error(error, &request_id),
    };
    let Ok(mut client) = state.db.acquire().await else {
        return http::database_error(&request_id);
    };
    let response = roles_remove_inner(&mut client, deltas, &request_id).await;
    state.db.release(client).await;
    response
}
