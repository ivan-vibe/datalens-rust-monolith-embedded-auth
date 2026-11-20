//! `appAuth` middleware — port of `src/components/auth/middlewares/app-auth.ts`.
//!
//! Tokens are verified as PS256 JWTs against `AUTH_TOKEN_PUBLIC_KEY`. The login
//! is taken from `payload.userId`, exactly like the TypeScript implementation
//! (`res.locals.login = payload.userId`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::Response;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{AppState, UserInfo};

const DL_AUTH_HEADER_KEY: &str = "Bearer";
const ACCESS_TOKEN_TYPE_SERVICE_ACCOUNT: &str = "serviceAccount";

#[derive(Debug, Clone, Deserialize)]
pub struct TokenPayload {
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    /// Kept for the future write endpoints (`ctx.get('info').user.sessionId`).
    #[allow(dead_code)]
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    #[serde(rename = "type", default)]
    pub token_type: Option<String>,
    /// `exp` claim; cached validations are re-checked against it.
    #[serde(default)]
    pub exp: Option<u64>,
}

/// A validated token is reused for this long. PS256 verification is the most expensive
/// per-request work on the US path (the data-api reads US two or three times per chart),
/// and the short window keeps revocation latency bounded.
const TOKEN_CACHE_TTL: Duration = Duration::from_secs(60);
const TOKEN_CACHE_MAX: usize = 4096;

fn token_cache() -> &'static Mutex<HashMap<u64, (TokenPayload, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, (TokenPayload, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn token_hash(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn decode_cached(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
) -> Result<TokenPayload, jsonwebtoken::errors::Error> {
    let hash = token_hash(token);
    if let Ok(mut cache) = token_cache().lock() {
        if let Some((payload, validated_at)) = cache.get(&hash) {
            let fresh = validated_at.elapsed() < TOKEN_CACHE_TTL;
            let unexpired = payload.exp.map(|exp| exp > unix_now()).unwrap_or(true);
            if fresh && unexpired {
                return Ok(payload.clone());
            }
            cache.remove(&hash);
        }
    }
    let payload = decode::<TokenPayload>(token, key, validation)?.claims;
    if let Ok(mut cache) = token_cache().lock() {
        if cache.len() >= TOKEN_CACHE_MAX {
            cache.clear();
        }
        cache.insert(hash, (payload.clone(), Instant::now()));
    }
    Ok(payload)
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user: UserInfo,
    /// Full `res.locals` equivalent available to the handlers.
    #[allow(dead_code)]
    pub roles: Vec<String>,
    #[allow(dead_code)]
    pub token_type: Option<String>,
    #[allow(dead_code)]
    pub access_token: String,
}

fn unauthorized(reason: &str) -> ApiError {
    tracing::debug!(reason, "CHECK_ACCESS_TOKEN_ERROR");
    ApiError::unauthorized()
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !state.config.auth_enabled {
        return Ok(next.run(request).await);
    }

    // express marks `/private/*` routes with `authPolicy: disabled`: they do not
    // require a Bearer token, only the master token (checked in `api::compat`).
    if request.uri().path().starts_with("/private") {
        return Ok(next.run(request).await);
    }

    let raw_header = request
        .headers()
        .get(AUTHORIZATION)
        .ok_or_else(|| unauthorized("missing authorization header"))?;

    let authorization = raw_header
        .to_str()
        .map_err(|_| unauthorized("non-ascii authorization header"))?;

    // `authorization.slice(DL_AUTH_HEADER_KEY.length + 1)`
    let access_token = authorization
        .get(DL_AUTH_HEADER_KEY.len() + 1..)
        .unwrap_or_default()
        .trim()
        .to_owned();

    if access_token.is_empty() {
        return Err(unauthorized("empty access token"));
    }

    let public_key = state
        .config
        .auth_public_key
        .as_deref()
        .ok_or_else(|| unauthorized("AUTH_TOKEN_PUBLIC_KEY is not configured"))?;

    let decoding_key = DecodingKey::from_rsa_pem(public_key.as_bytes())
        .map_err(|_| unauthorized("invalid public key"))?;

    let mut validation = Validation::new(Algorithm::PS256);
    // `jsonwebtoken` (Node) only validates `exp` when the claim is present.
    validation.required_spec_claims.clear();
    validation.validate_aud = false;

    let payload = decode_cached(&access_token, &decoding_key, &validation)
        .map_err(|error| unauthorized(&format!("{error}")))?;
    let login = payload.user_id.clone();

    request.extensions_mut().insert(AuthUser {
        user: UserInfo {
            user_id: payload.user_id,
            login,
        },
        roles: payload.roles.unwrap_or_default(),
        token_type: payload
            .token_type
            .filter(|kind| kind != ACCESS_TOKEN_TYPE_SERVICE_ACCOUNT),
        access_token: access_token.to_owned(),
    });

    Ok(next.run(request).await)
}
