//! Environment configuration. Names mirror the Node service where the value has a
//! protocol effect (`TOKEN_*`, TTLs, cookie flags); `AUTH_*` names are used for the
//! deployment-specific knobs.

use anyhow::{bail, Context, Result};
use std::time::Duration;

pub const DEFAULT_DSN: &str = "postgres://pg-user:postgres@127.0.0.1:28185/pg-auth-db";

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub dsn: String,
    pub access_ttl: i64,
    pub refresh_ttl: i64,
    pub session_ttl: i64,
    pub cookie_name: String,
    pub cookie_secure: bool,
    /// `Strict` (Node default), `Lax` or `None`.
    pub cookie_same_site: String,
    pub cookie_domain: Option<String>,
    pub cookie_max_age: i64,
    pub signup_disabled: bool,
    pub manage_local_users_disabled: bool,
    pub default_role: String,
    /// Comma-separated master tokens for the `/private/v1/*` routes (Node `MASTER_TOKEN`,
    /// which `getEnvTokenVariable` turns into a list and matches exactly). Empty = private
    /// routes disabled with the reference "No master token in config" response.
    pub master_tokens: Vec<String>,
    pub db_pool_size: usize,
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_opt_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_i64(name: &str, default: i64) -> Result<i64> {
    match env_opt_string(name) {
        None => Ok(default),
        Some(value) => value
            .parse::<i64>()
            .with_context(|| format!("{name} must be an integer, got {value:?}"))
            .and_then(|parsed| {
                if parsed > 0 {
                    Ok(parsed)
                } else {
                    bail!("{name} must be positive, got {parsed}")
                }
            }),
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    match env_opt_string(name) {
        None => default,
        Some(value) => matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    }
}

/// `AUTH_MASTER_TOKEN` (comma-separated) or the Node-compatible `MASTER_TOKEN` name.
fn master_tokens() -> Vec<String> {
    ["AUTH_MASTER_TOKEN", "MASTER_TOKEN"]
        .iter()
        .find_map(|name| env_opt_string(name))
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn same_site(raw: &str) -> Result<String> {
    match raw.to_ascii_lowercase().as_str() {
        "strict" => Ok("Strict".to_string()),
        "lax" => Ok("Lax".to_string()),
        "none" => Ok("None".to_string()),
        other => bail!("AUTH_COOKIE_SAME_SITE must be strict|lax|none, got {other:?}"),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port = match env_opt_string("AUTH_PORT").or_else(|| env_opt_string("PORT")) {
            None => 8097,
            Some(value) => value
                .parse::<u16>()
                .with_context(|| format!("AUTH_PORT/PORT must be a port, got {value:?}"))?,
        };
        let refresh_ttl = env_i64("AUTH_REFRESH_TOKEN_TTL", 60 * 60 * 24 * 10)?;
        Ok(Self {
            port,
            dsn: env_string("AUTH_POSTGRES_DSN", DEFAULT_DSN),
            access_ttl: env_i64("AUTH_ACCESS_TOKEN_TTL", 60 * 15)?,
            refresh_ttl,
            session_ttl: env_i64("AUTH_SESSION_TTL", 60 * 60 * 24 * 30)?,
            cookie_name: env_string("AUTH_COOKIE_NAME", "auth"),
            cookie_secure: env_bool("AUTH_COOKIE_SECURE", false),
            cookie_same_site: same_site(&env_string("AUTH_COOKIE_SAME_SITE", "strict"))?,
            cookie_domain: env_opt_string("AUTH_COOKIE_DOMAIN"),
            // Node: `maxAge = refreshTokenTTL * 1000 + ONE_HOUR`.
            cookie_max_age: env_i64("AUTH_COOKIE_MAX_AGE", refresh_ttl + 3600)?,
            // The native compose file sets AUTH_SIGNUP_DISABLED=true; keep the safe default.
            signup_disabled: env_bool("AUTH_SIGNUP_DISABLED", true),
            manage_local_users_disabled: env_bool("AUTH_MANAGE_LOCAL_USERS_DISABLED", false),
            default_role: env_string("AUTH_DEFAULT_ROLE", "datalens.viewer"),
            master_tokens: master_tokens(),
            db_pool_size: env_i64("AUTH_DB_POOL_SIZE", 4)? as usize,
        })
    }

    pub fn cookie_max_age_duration(&self) -> Duration {
        Duration::from_secs(self.cookie_max_age.max(0) as u64)
    }

    pub fn access_token_exp_cookie_name(&self) -> String {
        format!("{}_exp", self.cookie_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_site_is_normalized_like_the_cookie_serializer() {
        assert_eq!(same_site("strict").unwrap(), "Strict");
        assert_eq!(same_site("LAX").unwrap(), "Lax");
        assert_eq!(same_site("none").unwrap(), "None");
        assert!(same_site("nonsense").is_err());
    }
}
