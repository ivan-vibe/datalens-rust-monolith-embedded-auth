//! `AUTH__TYPE=NATIVE`: JWT validation matching `dl_auth_native`.
//!
//! The Python data-api validates a PS256 (by default) JWT from the `Authorization` header,
//! extracts `userId`, `roles` and an optional `groups` claim, and gates the data routes.
//! The compiler uses the identity for RLS; `us` is still read with the service master token,
//! and per-object ACLs are left to the UI/control-api.
//!
//! Errors are `401 ERR.DS_API.REQUEST.AUTH`; `/ping` and `/ping_ready` stay public.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dl_query::identity::{with_identity, Identity};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::{ApiError, AppState, HttpError};

tokio::task_local! {
    /// The caller's bearer token for the duration of one request, so the `us` client can read
    /// the dataset on behalf of the user (`UsClient::get_entry_as`) instead of the service
    /// master token. The direct in-process chart path does not set it yet.
    static USER_BEARER: String;
}

/// The bearer token of the request being handled, if the auth middleware installed one.
pub fn current_bearer() -> Option<String> {
    USER_BEARER.try_with(Clone::clone).ok()
}

/// Run `future` with `token` visible to [`current_bearer`].
pub async fn with_bearer<F: std::future::Future>(token: String, future: F) -> F::Output {
    USER_BEARER.scope(token, future).await
}

/// A validated JWT payload (`dl_auth_native.token.Payload`).
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    #[serde(rename = "userId")]
    pub user_id: String,
    /// Unix timestamp; `jsonwebtoken` rejects a token whose `exp` is in the past.
    pub exp: usize,
    #[serde(default)]
    pub roles: Vec<String>,
    /// Optional `groups` claim; the open-source auth service does not issue one, so group RLS
    /// rules do not match unless the deployment adds it.
    #[serde(default)]
    pub groups: Vec<String>,
}

/// The key and algorithm `AUTH__TYPE=NATIVE` validates with.
#[derive(Clone, Debug)]
pub struct NativeAuth {
    key: DecodingKey,
    algorithm: Algorithm,
}

impl NativeAuth {
    /// `AUTH__JWT_KEY` (PEM public key) + `AUTH__JWT_ALGORITHM` (default `PS256`), the names
    /// the upstream Compose file uses.
    pub fn from_env() -> Result<Self, String> {
        let key = std::env::var("AUTH__JWT_KEY")
            .map_err(|_| "AUTH__TYPE=NATIVE requires AUTH__JWT_KEY".to_string())?;
        // Compose passes the PEM as one line with `\n` escapes, as the Python settings parser
        // expects; accept both that and a real multi-line value.
        let key = key.replace("\\n", "\n");
        let algorithm =
            std::env::var("AUTH__JWT_ALGORITHM").unwrap_or_else(|_| "PS256".to_string());
        Self::from_pem(key.as_bytes(), &algorithm)
    }

    pub fn from_pem(pem: &[u8], algorithm: &str) -> Result<Self, String> {
        let algorithm = parse_algorithm(algorithm)?;
        let key = DecodingKey::from_rsa_pem(pem)
            .map_err(|error| format!("AUTH__JWT_KEY is not a PEM public key: {error}"))?;
        Ok(Self { key, algorithm })
    }

    /// Validate the signature and `exp`; returns the claims on success.
    pub fn validate(&self, token: &str) -> Result<Claims, String> {
        let mut validation = Validation::new(self.algorithm);
        validation.validate_exp = true;
        decode::<Claims>(token, &self.key, &validation)
            .map(|data| data.claims)
            .map_err(|error| error.to_string())
    }
}

fn parse_algorithm(raw: &str) -> Result<Algorithm, String> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "PS256" => Ok(Algorithm::PS256),
        "PS384" => Ok(Algorithm::PS384),
        "PS512" => Ok(Algorithm::PS512),
        "RS256" => Ok(Algorithm::RS256),
        "RS384" => Ok(Algorithm::RS384),
        "RS512" => Ok(Algorithm::RS512),
        other => Err(format!(
            "unsupported AUTH__JWT_ALGORITHM {other}: expected PS256/PS384/PS512/RS256/RS384/RS512"
        )),
    }
}

/// Validate the JWT and run the request with the bearer token visible to `dl_us`.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return next.run(request).await;
    };
    // `RequiredResourceCommon.SKIP_AUTH` of `UnistatView` / `MetricsView`, and the probes.
    if matches!(
        request.uri().path(),
        "/ping" | "/ping_ready" | "/metrics" | "/unistat" | "/unistat/"
    ) {
        return next.run(request).await;
    }
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized("User access token header is missing");
    };
    let token = token.to_string();
    let validation_started = std::time::Instant::now();
    let validation = auth.validate(&token);
    dl_api::trace::record_work("auth", validation_started.elapsed());
    match validation {
        Ok(claims) => {
            let identity = Identity {
                user_id: claims.user_id,
                groups: claims.groups,
                roles: claims.roles,
            };
            with_bearer(token, with_identity(identity, next.run(request))).await
        }
        Err(error) => unauthorized(&format!("Invalid user access token: {error}")),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.splitn(2, ' ');
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    parts
        .next()
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn unauthorized(message: &str) -> Response {
    HttpError(ApiError::new(401, "ERR.DS_API.REQUEST.AUTH", message)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;

    const PUBLIC_KEY: &str = include_str!("../tests/fixtures/test-public.pem");
    const PRIVATE_KEY: &str = include_str!("../tests/fixtures/test-private.pem");

    #[derive(Serialize)]
    struct TestClaims {
        #[serde(rename = "userId")]
        user_id: String,
        exp: usize,
        roles: Vec<String>,
    }

    fn auth() -> NativeAuth {
        NativeAuth::from_pem(PUBLIC_KEY.as_bytes(), "PS256").unwrap()
    }

    fn token(exp: usize) -> String {
        let key = EncodingKey::from_rsa_pem(PRIVATE_KEY.as_bytes()).unwrap();
        let claims = TestClaims {
            user_id: "user-1".to_string(),
            exp,
            roles: vec!["viewer".to_string()],
        };
        encode(&Header::new(Algorithm::PS256), &claims, &key).unwrap()
    }

    #[test]
    fn a_valid_token_yields_the_user_and_roles() {
        let claims = auth().validate(&token(usize::MAX)).unwrap();
        assert_eq!(claims.user_id, "user-1");
        assert_eq!(claims.roles, vec!["viewer"]);
    }

    #[test]
    fn an_expired_token_is_rejected() {
        assert!(auth().validate(&token(1)).is_err());
    }

    #[test]
    fn a_tampered_token_is_rejected() {
        let mut raw = token(usize::MAX);
        raw.push_str("tampered");
        assert!(auth().validate(&raw).is_err());
    }

    #[test]
    fn only_known_algorithms_are_accepted() {
        assert!(parse_algorithm("PS256").is_ok());
        assert!(parse_algorithm("rs256").is_ok());
        assert!(parse_algorithm("HS256").is_err());
    }

    #[tokio::test]
    async fn the_bearer_is_scoped_to_the_request() {
        assert!(current_bearer().is_none());
        let seen = with_bearer("user-jwt".to_string(), async { current_bearer() }).await;
        assert_eq!(seen.as_deref(), Some("user-jwt"));
        assert!(current_bearer().is_none());
    }

    #[test]
    fn a_bearer_token_is_read_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "bearer user-jwt".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("user-jwt"));
        headers.insert(AUTHORIZATION, "Basic x".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }
}
