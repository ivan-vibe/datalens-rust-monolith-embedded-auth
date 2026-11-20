//! Route guards, matching the Node middleware order:
//! `appAuth` (Bearer) → `checkPermissions` (permission, then route checks), plus the
//! `resolveSpecialTokens` master-token gate for `/private/*`.

use axum::http::HeaderMap;
use axum::response::Response;
use tokio_postgres::Client;

use crate::db;
use crate::http;
use crate::ids;
use crate::validation::{ROLE_ADMIN, ROLE_CREATOR, ROLE_EDITOR, ROLE_VIEWER, ROLE_VISITOR};
use crate::{AppState, Config};

pub struct AuthUser {
    pub user_id: i64,
    pub roles: Vec<String>,
}

/// `appAuth`: verify the PS256 access token and decode the user/session ids. Anything wrong is
/// the reference `401 Unauthorized access` text response.
pub fn auth_user(state: &AppState, headers: &HeaderMap, request_id: &str) -> Result<AuthUser, Response> {
    let Some(token) = http::bearer_token(headers).filter(|token| !token.is_empty()) else {
        return Err(http::unauthorized(request_id));
    };
    let Ok(claims) = state.jwt.verify_access(token) else {
        return Err(http::unauthorized(request_id));
    };
    let Ok(user_id) = ids::decode_id_lenient(&claims.user_id) else {
        return Err(http::unauthorized(request_id));
    };
    if ids::decode_id_lenient(&claims.session_id).is_err() {
        return Err(http::unauthorized(request_id));
    }
    Ok(AuthUser {
        user_id,
        roles: claims.roles,
    })
}

/// `Permission.InstanceUse`: any of the five known roles in the token.
pub fn instance_use(user: &AuthUser, request_id: &str) -> Result<(), Response> {
    let allowed = [ROLE_ADMIN, ROLE_EDITOR, ROLE_VIEWER, ROLE_VISITOR, ROLE_CREATOR];
    if user.roles.iter().any(|role| allowed.contains(&role.as_str())) {
        Ok(())
    } else {
        Err(http::access_denied(request_id))
    }
}

/// `Permission.Manage`: the token must carry `datalens.admin` **and** the database roles of
/// that user must still grant it (`introspectUserPermission`).
pub async fn manage_permission(
    client: &Client,
    user: &AuthUser,
    request_id: &str,
) -> Result<(), Response> {
    if !user.roles.iter().any(|role| role == ROLE_ADMIN) {
        return Err(http::access_denied(request_id));
    }
    match db::user_roles(client, user.user_id).await {
        Ok(roles) if roles.iter().any(|role| role == ROLE_ADMIN) => Ok(()),
        Ok(_) => Err(http::access_denied(request_id)),
        Err(error) => {
            tracing::warn!(%error, "failed to introspect user roles");
            Err(http::database_error(request_id))
        }
    }
}

/// `RouteCheck.ManageLocalUsers`.
pub fn manage_local_users(config: &Config, request_id: &str) -> Result<(), Response> {
    if config.manage_local_users_disabled {
        return Err(http::json_value(
            axum::http::StatusCode::FORBIDDEN,
            serde_json::json!({
                "message": "Local user management is disabled",
                "code": "AUTH.MANAGE_LOCAL_USERS_DISABLED",
            }),
            request_id,
        ));
    }
    Ok(())
}

/// `resolveSpecialTokens.resolvePrivateRoute`: exact match against one of the configured
/// master tokens (`MASTER_TOKEN` is a comma-separated list).
pub fn master_token(config: &Config, headers: &HeaderMap, request_id: &str) -> Result<(), Response> {
    if config.master_tokens.is_empty() {
        return Err(http::json_value(
            axum::http::StatusCode::FORBIDDEN,
            serde_json::json!({"error": "No master token in config"}),
            request_id,
        ));
    }
    let presented = headers
        .get("x-dl-auth-master-token")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());
    match presented {
        Some(token) if config.master_tokens.iter().any(|known| known == token) => Ok(()),
        _ => Err(http::json_value(
            axum::http::StatusCode::FORBIDDEN,
            serde_json::json!({"error": "Private API call denied"}),
            request_id,
        )),
    }
}
