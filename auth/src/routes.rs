//! Auth routes and server wiring. The user/user-management routes live in
//! [`crate::users_api`]; this module owns signin/refresh/logout/profile, health probes and the
//! router/404 glue.

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::cookies::{self, AuthCookie};
use crate::db::{self, SignupError, Tokens};
use crate::http::{self, ParsedBody};
use crate::ids::decode_id;
use crate::models::{ProfileResponse, UserOut};
use crate::password;
use crate::validation;
use crate::{AppState, Config};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/ping", get(ping))
        .route("/ping-db", get(ping_db))
        .route("/ping-db-primary", get(ping_db_primary))
        .route("/signin-fail", get(signin_fail))
        .route("/signin", post(signin))
        .route("/refresh", post(refresh))
        .route("/logout", get(logout))
        .route("/signup", post(signup))
        .route("/v1/users/me/profile", get(profile))
        .merge(crate::users_api::router())
        .fallback(http::not_found)
        // Express answers 404 (not 405) for a known path with the wrong method, e.g. GET /signin.
        .method_not_allowed_fallback(http::not_found)
        // Express body-parser is configured with `limit: '50mb'` in the reference service.
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state)
}

fn signin_fail_redirect(request_id: &str) -> Response {
    let mut redirect = http::text_response(
        StatusCode::FOUND,
        "text/plain; charset=utf-8",
        "Found. Redirecting to /signin-fail",
        request_id,
    );
    let headers = redirect.headers_mut();
    headers.insert(axum::http::header::LOCATION, HeaderValue::from_static("/signin-fail"));
    headers.insert(axum::http::header::VARY, HeaderValue::from_static("Accept"));
    redirect
}

fn refresh_error(request_id: &str, message: &str) -> Response {
    http::json_value(
        StatusCode::UNAUTHORIZED,
        json!({"code": "AUTH.NEED_RESET", "message": message}),
        request_id,
    )
}

// ---------------------------------------------------------------------------------------
// health probes / home / signin-fail
// ---------------------------------------------------------------------------------------

async fn home(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // The Node `/` route runs under the default `authPolicy: required`.
    let request_id = http::request_id(&headers);
    let Some(token) = http::bearer_token(&headers).filter(|token| !token.is_empty()) else {
        return http::unauthorized(&request_id);
    };
    match state.jwt.verify_access(token) {
        Ok(claims) if decode_id(&claims.user_id).is_some() => http::text_response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            "hey",
            &request_id,
        ),
        _ => http::unauthorized(&request_id),
    }
}

async fn ping(headers: HeaderMap) -> Response {
    http::json_value(
        StatusCode::OK,
        json!({"result": "pong"}),
        &http::request_id(&headers),
    )
}

async fn signin_fail(headers: HeaderMap) -> Response {
    http::json_value(
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
        &http::request_id(&headers),
    )
}

// ---------------------------------------------------------------------------------------
// client IP / user agent (request-ip semantics)
// ---------------------------------------------------------------------------------------

fn user_agent(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("Unknown")
        .to_string()
}

fn strip_port(candidate: &str) -> &str {
    // request-ip: exactly one ':' means `ip:port`; IPv6 addresses are left alone.
    if candidate.contains(':') && candidate.split(':').count() == 2 {
        candidate.split(':').next().unwrap_or(candidate)
    } else {
        candidate
    }
}

fn valid_ip(candidate: &str) -> Option<String> {
    candidate
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| ip.to_string())
}

pub(crate) fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> Option<String> {
    let text_header = |name: &str| -> Option<&str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    };
    if let Some(value) = text_header("x-client-ip") {
        if let Some(ip) = valid_ip(value.trim()) {
            return Some(ip);
        }
    }
    if let Some(value) = text_header("x-forwarded-for") {
        for part in value.split(',') {
            if let Some(ip) = valid_ip(strip_port(part.trim())) {
                return Some(ip);
            }
        }
    }
    for name in [
        "cf-connecting-ip",
        "fastly-client-ip",
        "true-client-ip",
        "x-real-ip",
        "x-cluster-client-ip",
        "x-forwarded",
        "forwarded-for",
        "forwarded",
        "x-appengine-user-ip",
    ] {
        if let Some(value) = text_header(name) {
            if let Some(ip) = valid_ip(value.trim()) {
                return Some(ip);
            }
        }
    }
    Some(peer.ip().to_string())
}

// ---------------------------------------------------------------------------------------
// /signin
// ---------------------------------------------------------------------------------------

async fn signin(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    let value = match http::parse_json_body(&headers, &body) {
        ParsedBody::Malformed => return http::malformed_json(&request_id),
        ParsedBody::NotJson => serde_json::Value::Null,
        ParsedBody::Value(value) => value,
    };
    let login = value.get("login").and_then(serde_json::Value::as_str).unwrap_or("");
    let password = value.get("password").and_then(serde_json::Value::as_str).unwrap_or("");
    // Passport-local validates only `z.string().min(1)` for both fields and turns any
    // failure into `done(null, false)` → redirect; complexity rules belong to /signup.
    if login.is_empty() || password.is_empty() {
        return signin_fail_redirect(&request_id);
    }

    let user_agent = user_agent(&headers);
    let user_ip = client_ip(&headers, peer);
    let Ok(mut client) = state.db.acquire().await else {
        return signin_fail_redirect(&request_id);
    };
    let result = signin_db(
        &state,
        &mut client,
        login,
        password,
        &user_agent,
        user_ip.as_deref(),
    )
    .await;
    state.db.release(client).await;
    match result {
        Ok(tokens) => auth_success(&state.config, &request_id, &tokens),
        Err(error) => {
            tracing::info!(%error, "signin failed");
            signin_fail_redirect(&request_id)
        }
    }
}

async fn signin_db(
    state: &AppState,
    client: &mut tokio_postgres::Client,
    login: &str,
    password: &str,
    user_agent: &str,
    user_ip: Option<&str>,
) -> anyhow::Result<Tokens> {
    let Some((user_id, Some(hash))) = db::find_login_user(client, login).await? else {
        anyhow::bail!("no local user for this login");
    };
    let password = password.to_string();
    let verified = tokio::task::spawn_blocking(move || password::verify_password(&hash, &password))
        .await
        .unwrap_or(false);
    if !verified {
        anyhow::bail!("incorrect password");
    }
    db::signin_tokens(
        client,
        &state.jwt,
        &state.config,
        user_id,
        user_agent,
        user_ip,
    )
    .await
}

fn auth_success(config: &Config, request_id: &str, tokens: &Tokens) -> Response {
    let cookie = AuthCookie {
        access_token: tokens.access.clone(),
        refresh_token: tokens.refresh.clone(),
    };
    let set_cookies = cookies::auth_set_cookie_headers(config, &cookie, tokens.access_exp);
    let body = http::json_value(StatusCode::OK, json!({"done": true}), request_id);
    http::with_set_cookies(body, &set_cookies)
}

// ---------------------------------------------------------------------------------------
// /refresh, /logout
// ---------------------------------------------------------------------------------------

async fn refresh(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let request_id = http::request_id(&headers);
    let cookie_header = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let Some(cookie) = cookies::parse_auth_cookie(cookie_header, &state.config) else {
        return refresh_error(&request_id, "No refreshToken");
    };
    let Some(refresh_token) = cookie.refresh_token.as_deref().filter(|token| !token.is_empty())
    else {
        return refresh_error(&request_id, "No refreshToken");
    };
    let Ok(claims) = state.jwt.verify_refresh(refresh_token) else {
        return refresh_error(&request_id, "Can't refresh tokens");
    };

    let user_ip = client_ip(&headers, peer);
    let Ok(mut client) = state.db.acquire().await else {
        return refresh_error(&request_id, "Can't refresh tokens");
    };
    let result = db::refresh_tokens(
        &mut client,
        &state.jwt,
        &state.config,
        &claims,
        user_ip.as_deref(),
    )
    .await;
    state.db.release(client).await;
    match result {
        Ok(tokens) => auth_success(&state.config, &request_id, &tokens),
        Err(error) => {
            tracing::info!(%error, "refresh failed");
            refresh_error(&request_id, "Can't refresh tokens")
        }
    }
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let request_id = http::request_id(&headers);
    let cookie_header = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok());
    if let Some(cookie) = cookies::parse_auth_cookie(cookie_header, &state.config) {
        if let Some(refresh_token) = cookie.refresh_token.as_deref().filter(|token| !token.is_empty()) {
            // `JwtAuth.closeSession` swallows verification/DB errors; logout always succeeds.
            if let Ok(claims) = state.jwt.verify_refresh(refresh_token) {
                if let Ok(client) = state.db.acquire().await {
                    if let Err(error) = db::close_session(&client, &claims).await {
                        tracing::warn!(%error, "failed to close auth session");
                    }
                    state.db.release(client).await;
                }
            }
        }
    }
    let clear = cookies::clear_auth_cookie_headers(&state.config);
    let body = http::json_value(StatusCode::OK, json!({"done": true}), &request_id);
    http::with_set_cookies(body, &clear)
}

// ---------------------------------------------------------------------------------------
// GET /v1/users/me/profile
// ---------------------------------------------------------------------------------------

async fn profile(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let request_id = http::request_id(&headers);
    let Some(token) = http::bearer_token(&headers).filter(|token| !token.is_empty()) else {
        return http::unauthorized(&request_id);
    };
    let Ok(claims) = state.jwt.verify_access(token) else {
        return http::unauthorized(&request_id);
    };
    let Some(user_id) = decode_id(&claims.user_id) else {
        return http::unauthorized(&request_id);
    };
    let Ok(client) = state.db.acquire().await else {
        return http::internal_error(&request_id);
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
        Ok(None) => http::json_value(
            StatusCode::NOT_FOUND,
            json!({"code": "AUTH.USER_NOT_EXISTS", "message": "The user doesn't exist"}),
            &request_id,
        ),
        Err(error) => {
            tracing::warn!(%error, "profile query failed");
            http::internal_error(&request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// /signup (disabled by default; full implementation for AUTH_SIGNUP_DISABLED=false)
// ---------------------------------------------------------------------------------------

async fn signup(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_id = http::request_id(&headers);
    // `checkPermissions` runs before the controller in the Node service: the disabled
    // checks win over body validation.
    if state.config.manage_local_users_disabled {
        return http::json_value(
            StatusCode::FORBIDDEN,
            json!({
                "message": "Local user management is disabled",
                "code": "AUTH.MANAGE_LOCAL_USERS_DISABLED",
            }),
            &request_id,
        );
    }
    if state.config.signup_disabled {
        return http::json_value(
            StatusCode::FORBIDDEN,
            json!({"message": "Signup is disabled", "code": "AUTH.SIGNUP_DISABLED"}),
            &request_id,
        );
    }

    let body = match http::parse_json_body(&headers, &body) {
        ParsedBody::Malformed => return http::malformed_json(&request_id),
        ParsedBody::NotJson => None,
        ParsedBody::Value(value) => Some(value),
    };
    let value = body.unwrap_or(serde_json::Value::Null);
    if let Err(issues) = validation::validate_signup_body(Some(&value)) {
        return http::validation_error(&request_id, issues);
    }
    let login = value["login"].as_str().unwrap_or_default().to_string();
    let password = value["password"].as_str().unwrap_or_default().to_string();
    let email = value.get("email").and_then(serde_json::Value::as_str);
    let first_name = value.get("firstName").and_then(serde_json::Value::as_str);
    let last_name = value.get("lastName").and_then(serde_json::Value::as_str);

    let password_for_hash = password.clone();
    let Ok(Some(password_hash)) =
        tokio::task::spawn_blocking(move || password::hash_password(&password_for_hash)).await
    else {
        tracing::warn!("password hashing failed (getrandom)");
        return http::internal_error(&request_id);
    };

    let user_agent = user_agent(&headers);
    let user_ip = client_ip(&headers, peer);
    let Ok(mut client) = state.db.acquire().await else {
        return http::internal_error(&request_id);
    };
    let input = db::SignupInput {
        login: &login,
        password_hash: &password_hash,
        email,
        first_name,
        last_name,
        user_agent: &user_agent,
        user_ip: user_ip.as_deref(),
    };
    let result = db::signup(&mut client, &state.jwt, &state.config, &input).await;
    state.db.release(client).await;

    match result {
        Ok(tokens) => auth_success(&state.config, &request_id, &tokens),
        Err(SignupError::Exists) => http::json_value(
            StatusCode::CONFLICT,
            json!({"code": "AUTH.USER_ALREADY_EXISTS", "message": "The user already exists"}),
            &request_id,
        ),
        Err(SignupError::UniqueViolation) => http::json_value(
            StatusCode::BAD_REQUEST,
            json!({"code": "AUTH.DB.UNIQUE_VIOLATION", "message": "The entity already exists"}),
            &request_id,
        ),
        Err(SignupError::Other(error)) => {
            tracing::warn!(%error, "signup failed");
            http::internal_error(&request_id)
        }
    }
}

// ---------------------------------------------------------------------------------------
// DB health probes
// ---------------------------------------------------------------------------------------

async fn ping_db(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let request_id = http::request_id(&headers);
    let Ok(client) = state.db.acquire().await else {
        return http::json_value(StatusCode::BAD_GATEWAY, json!({"result": false}), &request_id);
    };
    let result = db::ping(&client).await;
    state.db.release(client).await;
    match result {
        Ok(()) => http::json_value(StatusCode::OK, json!({"result": "pong-db"}), &request_id),
        Err(error) => {
            tracing::warn!(%error, "ping-db failed");
            http::json_value(StatusCode::BAD_GATEWAY, json!({"result": false}), &request_id)
        }
    }
}

async fn ping_db_primary(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let request_id = http::request_id(&headers);
    let Ok(client) = state.db.acquire().await else {
        return http::json_value(
            StatusCode::BAD_GATEWAY,
            json!({"result": "db primary is not available"}),
            &request_id,
        );
    };
    let result = db::ping(&client).await;
    state.db.release(client).await;
    match result {
        Ok(()) => http::json_value(
            StatusCode::OK,
            json!({"result": "db primary is ok"}),
            &request_id,
        ),
        Err(error) => {
            tracing::warn!(%error, "ping-db-primary failed");
            http::json_value(
                StatusCode::BAD_GATEWAY,
                json!({"result": "db primary is not available"}),
                &request_id,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn client_ip_prefers_forwarded_headers() {
        let peer: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.7, 10.0.0.1"),
        );
        assert_eq!(client_ip(&headers, peer).as_deref(), Some("203.0.113.7"));

        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7:8080"));
        assert_eq!(client_ip(&headers, peer).as_deref(), Some("203.0.113.7"));

        let empty = HeaderMap::new();
        assert_eq!(client_ip(&empty, peer).as_deref(), Some("127.0.0.1"));
    }
}
