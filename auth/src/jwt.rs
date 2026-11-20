//! PS256 JWT issuing/validation with the exact claim shapes of the Node service.
//!
//! Node `jsonwebtoken.sign` adds `iat` from its own clock and sets `exp = iat + TTL`; the
//! access token carries `userId/sessionId/roles`, the refresh token carries
//! `refreshTokenId/userId/sessionId` and **no `exp`**. IDs are the encoded (13-char) form,
//! so claims are directly usable by control-api, meta-manager, US and the UI.

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub roles: Vec<String>,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshClaims {
    #[serde(rename = "refreshTokenId")]
    pub refresh_token_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub iat: i64,
}

pub struct Jwt {
    encoding: EncodingKey,
    decoding: DecodingKey,
    access_ttl: i64,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Compose passes PEMs as a single line with `\n` escapes (the Node `getEnvCert` parser
/// accepts that); real multi-line values are accepted too.
pub fn normalize_pem(value: &str) -> String {
    value.replace("\\n", "\n")
}

impl Jwt {
    pub fn new(private_key: &str, public_key: &str, access_ttl: i64) -> Result<Self> {
        let encoding = EncodingKey::from_rsa_pem(normalize_pem(private_key).as_bytes())
            .context("TOKEN_PRIVATE_KEY is not a PEM RSA private key")?;
        let decoding = DecodingKey::from_rsa_pem(normalize_pem(public_key).as_bytes())
            .context("TOKEN_PUBLIC_KEY is not a PEM RSA public key")?;
        Ok(Self {
            encoding,
            decoding,
            access_ttl,
        })
    }

    pub fn sign_access(
        &self,
        user_id: &str,
        session_id: &str,
        roles: Vec<String>,
    ) -> Result<(String, i64)> {
        let iat = unix_now();
        let exp = iat + self.access_ttl;
        let claims = AccessClaims {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            roles,
            iat,
            exp,
        };
        let token = encode(&Header::new(Algorithm::PS256), &claims, &self.encoding)
            .context("sign access token")?;
        Ok((token, exp))
    }

    pub fn sign_refresh(
        &self,
        refresh_token_id: &str,
        user_id: &str,
        session_id: &str,
    ) -> Result<String> {
        let claims = RefreshClaims {
            refresh_token_id: refresh_token_id.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            iat: unix_now(),
        };
        encode(&Header::new(Algorithm::PS256), &claims, &self.encoding)
            .context("sign refresh token")
    }

    /// Node `jsonwebtoken.verify(token, publicKey, { algorithms: ['PS256'] })`: `exp` is
    /// validated only when present, nothing else is required.
    fn validation(validate_exp: bool) -> Validation {
        let mut validation = Validation::new(Algorithm::PS256);
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        validation.validate_exp = validate_exp;
        // jsonwebtoken defaults to 60 s of leeway; Node's jsonwebtoken uses 0.
        validation.leeway = 0;
        validation
    }

    pub fn verify_access(&self, token: &str) -> Result<AccessClaims> {
        decode::<AccessClaims>(token, &self.decoding, &Self::validation(true))
            .map(|data| data.claims)
            .map_err(|error| anyhow!("invalid access token: {error}"))
    }

    /// Refresh tokens have no `exp`, so expiry validation is off; the DB row is the
    /// authority on refresh-token lifetime (exactly like the Node service).
    pub fn verify_refresh(&self, token: &str) -> Result<RefreshClaims> {
        decode::<RefreshClaims>(token, &self.decoding, &Self::validation(false))
            .map(|data| data.claims)
            .map_err(|error| anyhow!("invalid refresh token: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE_KEY: &str = include_str!("../tests/fixtures/test-private.pem");
    const PUBLIC_KEY: &str = include_str!("../tests/fixtures/test-public.pem");

    fn jwt() -> Jwt {
        Jwt::new(PRIVATE_KEY, PUBLIC_KEY, 900).expect("test keys")
    }

    #[test]
    fn access_round_trip_keeps_claims_and_ttl() {
        let jwt = jwt();
        let (token, exp) = jwt
            .sign_access("6htfqmn1ocuip", "q3cm18c7bf4q9", vec!["datalens.admin".into()])
            .unwrap();
        let claims = jwt.verify_access(&token).unwrap();
        assert_eq!(claims.user_id, "6htfqmn1ocuip");
        assert_eq!(claims.session_id, "q3cm18c7bf4q9");
        assert_eq!(claims.roles, vec!["datalens.admin"]);
        assert_eq!(claims.exp, exp);
        assert_eq!(exp - claims.iat, 900);
    }

    #[test]
    fn refresh_token_has_no_exp_and_validates() {
        let jwt = jwt();
        let token = jwt
            .sign_refresh("box7mtyruljku", "6htfqmn1ocuip", "q3cm18c7bf4q9")
            .unwrap();
        let claims = jwt.verify_refresh(&token).unwrap();
        assert_eq!(claims.refresh_token_id, "box7mtyruljku");
        assert_eq!(claims.user_id, "6htfqmn1ocuip");
        // A refresh token must not satisfy the access-token validator: it has no `exp`,
        // and an access token must not be accepted as a refresh token (different claims).
        assert!(jwt.verify_access(&token).is_err());
        let (access, _) = jwt
            .sign_access("6htfqmn1ocuip", "q3cm18c7bf4q9", vec![])
            .unwrap();
        assert!(jwt.verify_refresh(&access).is_err());
    }

    #[test]
    fn expired_and_tampered_tokens_are_rejected() {
        let jwt = jwt();
        let claims = AccessClaims {
            user_id: "6htfqmn1ocuip".into(),
            session_id: "q3cm18c7bf4q9".into(),
            roles: vec![],
            iat: unix_now() - 100,
            exp: unix_now() - 10,
        };
        let expired = encode(
            &Header::new(Algorithm::PS256),
            &claims,
            &EncodingKey::from_rsa_pem(normalize_pem(PRIVATE_KEY).as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(jwt.verify_access(&expired).is_err());

        let (token, _) = jwt.sign_access("6htfqmn1ocuip", "q3cm18c7bf4q9", vec![]).unwrap();
        let mut tampered = token.clone();
        tampered.push('x');
        assert!(jwt.verify_access(&tampered).is_err());
        assert!(jwt.verify_access("not.a.jwt").is_err());
    }

    #[test]
    fn composes_escaped_pem() {
        let one_line = PRIVATE_KEY.replace('\n', "\\n");
        assert!(Jwt::new(&one_line, &PUBLIC_KEY.replace('\n', "\\n"), 900).is_ok());
        assert!(Jwt::new("not a key", PUBLIC_KEY, 900).is_err());
    }
}
