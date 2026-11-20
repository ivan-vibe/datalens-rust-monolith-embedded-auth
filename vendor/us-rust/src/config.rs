//! Configuration — port of `src/configs/common.ts`, `src/configs/opensource/*`
//! and `src/components/gateway` (DSN resolution).

use std::env;

const DEFAULT_PORT: u16 = 8080;
const DEFAULT_TENANT_ID: &str = "common";
const DEFAULT_PG_PORT: u16 = 5432;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Primary (read-write) database.
    pub pg_primary_dsn: String,
    /// Replica (read-only) database; falls back to the primary when absent.
    pub pg_replica_dsn: String,
    pub pg_pool_max: u32,
    pub pg_query_timeout_ms: u64,
    pub app_name: String,
    pub auth_enabled: bool,
    pub auth_public_key: Option<String>,
    /// `config.tenantIdOverride` — `resolveTenantId` uses it for every request.
    pub tenant_id_override: String,
    /// `config.dlsEnabled` — when off (the open-source default) the service
    /// reports an empty `permissions` object, exactly like the Node build.
    pub dls_enabled: bool,
    /// `US_RS_HTTP_LOG=0` disables the pino-style per-request log lines (the
    /// Node service always logs; this is an operational escape hatch for
    /// throughput-sensitive deployments that do not need access logs).
    pub http_log: bool,
    /// `READ_ONLY_MODE` feature: mutating routes answer 451.
    pub read_only_mode: bool,
    /// `MASTER_TOKEN` — comma-separated static private API tokens.
    pub master_tokens: Vec<String>,
    /// `UI_/BI_MASTER_TOKEN_PUBLIC_KEY_*` — dynamic master token keys (RS256).
    pub dynamic_master_keys_ui: Vec<String>,
    pub dynamic_master_keys_bi: Vec<String>,
    /// `Feature.DynamicMasterTokenEnabled` (`configs/opensource/common.js`).
    pub dynamic_master_token_enabled: bool,
    /// `Feature.DynamicMasterTokenIsRequired`: static `MASTER_TOKEN` is refused.
    pub dynamic_master_token_is_required: bool,
    /// `US_RS_REQUEST_TIMEOUT_MS` (v2): 0/absent disables the HTTP timeout.
    pub request_timeout_ms: u64,
    /// `TRUST_PROXY` (v2): number of trusted proxies for `X-Forwarded-For`
    /// (express `expressTrustProxyNumber: 3`). 0 disables XFF handling.
    pub trust_proxy: u32,
}

fn env_string(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// `getEnvVariable` from `src/utils/env-utils.ts`: `<NAME>` or `<NAME>_FILE_PATH`.
fn env_variable(name: &str) -> Option<String> {
    if let Some(value) = env_string(name) {
        return Some(value);
    }

    let file_path = env_string(&format!("{name}_FILE_PATH"))?;
    std::fs::read_to_string(file_path).ok()
}

/// `getEnvCert` from `src/utils/env-utils.ts`.
fn unescape_pem(value: &str) -> String {
    value.replace("\\n", "\n")
}

fn env_bool(name: &str) -> Option<bool> {
    let raw = env_variable(name)?;
    let raw = raw.trim().to_ascii_lowercase();
    Some(matches!(raw.as_str(), "1" | "true" | "yes" | "on"))
}

fn env_u64(name: &str) -> Option<u64> {
    env_variable(name)?.trim().parse::<u64>().ok()
}

fn percent_encode(value: &str) -> String {
    const SAFE: &[u8] = b"-._~";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric() || SAFE.contains(byte);
        if is_unreserved {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Builds `postgres://user:password@host:port/database` for one host.
fn build_dsn(
    host: &str,
    port: u16,
    user: Option<&str>,
    password: Option<&str>,
    database: &str,
    ssl: bool,
) -> String {
    let credentials = match (user, password) {
        (Some(user), Some(password)) => {
            format!("{}:{}@", percent_encode(user), percent_encode(password))
        }
        (Some(user), None) => format!("{}@", percent_encode(user)),
        _ => String::new(),
    };

    if host.starts_with('/') {
        // Unix socket.
        return format!("postgres://{credentials}/{database}?host={host}&port={port}");
    }

    // `pg` uses `ssl=true`; sqlx spells the same thing `sslmode=require`
    // (encrypt, no certificate verification, like Node's `rejectUnauthorized: false`).
    let ssl = if ssl {
        "?sslmode=require"
    } else {
        "?sslmode=disable"
    };
    format!("postgres://{credentials}{host}:{port}/{database}{ssl}")
}

/// `getDsnList`: either an explicit `POSTGRES_DSN_LIST` or `POSTGRES_HOSTS`
/// combined with the usual libpq variables. First entry — primary, second —
/// replica (identical to the Node gateway).
fn resolve_dsn_list() -> Vec<String> {
    // `POSTGRES_RDS_CONFIG_PATH`: JSON file with host/port/dbname/username/password.
    if let Some(path) = env_variable("POSTGRES_RDS_CONFIG_PATH") {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                let host = value
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost");
                let port = value.get("port").and_then(|v| v.as_u64()).unwrap_or(5432) as u16;
                let database = value.get("dbname").and_then(|v| v.as_str()).unwrap_or("us");
                let user = value.get("username").and_then(|v| v.as_str());
                let password = value.get("password").and_then(|v| v.as_str());
                return vec![build_dsn(host, port, user, password, database, true)];
            }
        }
    }

    if let Some(raw) = env_variable("POSTGRES_DSN_LIST") {
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|dsn| !dsn.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if !list.is_empty() {
            return list;
        }
    }

    let hosts = env_variable("POSTGRES_HOSTS")
        .or_else(|| env_string("POSTGRES_HOST"))
        .unwrap_or_else(|| "localhost".to_owned());

    let port = env_u64("POSTGRES_PORT").unwrap_or(DEFAULT_PG_PORT as u64) as u16;
    // Node reads POSTGRES_USER_NAME / POSTGRES_USER_PASSWD / POSTGRES_DB_NAME;
    // the shorter names are kept as a fallback for local runs.
    let user = env_variable("POSTGRES_USER_NAME").or_else(|| env_variable("POSTGRES_USER"));
    let password =
        env_variable("POSTGRES_USER_PASSWD").or_else(|| env_variable("POSTGRES_PASSWORD"));
    let database = env_variable("POSTGRES_DB_NAME")
        .or_else(|| env_variable("POSTGRES_DATABASE"))
        .unwrap_or_else(|| "us".to_owned());
    // `pg` appends `?ssl=true` unless `POSTGRES_DISABLE_SSL` is set.
    let ssl = !env_bool("POSTGRES_DISABLE_SSL").unwrap_or(false);

    hosts
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(|host| {
            build_dsn(
                host,
                port,
                user.as_deref(),
                password.as_deref(),
                &database,
                ssl,
            )
        })
        .collect()
}

impl Config {
    pub fn from_env() -> Self {
        let dsn_list = resolve_dsn_list();
        let pg_primary_dsn = dsn_list
            .first()
            .cloned()
            .unwrap_or_else(|| "postgres://localhost:5432/us".to_owned());
        let pg_replica_dsn = dsn_list
            .get(1)
            .cloned()
            .unwrap_or_else(|| pg_primary_dsn.clone());

        Self {
            host: env_variable("HOST").unwrap_or_else(|| "0.0.0.0".to_owned()),
            port: env_u64("PORT").unwrap_or(DEFAULT_PORT as u64) as u16,
            pg_primary_dsn,
            pg_replica_dsn,
            // `src/db/init-db.ts` pins knex's pool max to 15.
            pg_pool_max: env_u64("PG_POOL_MAX")
                .or_else(|| env_u64("DATABASE_MAX_CONNECTIONS"))
                .unwrap_or(15) as u32,
            pg_query_timeout_ms: env_u64("POSTGRES_QUERY_TIMEOUT")
                .or_else(|| env_u64("DEFAULT_QUERY_TIMEOUT"))
                .unwrap_or(20_000),
            app_name: env_variable("APP_NAME").unwrap_or_else(|| "us".to_owned()),
            auth_enabled: env_bool("AUTH_ENABLED").unwrap_or(false),
            auth_public_key: env_variable("AUTH_TOKEN_PUBLIC_KEY").map(|key| unescape_pem(&key)),
            tenant_id_override: env_variable("TENANT_ID_OVERRIDE")
                .or_else(|| env_variable("DEFAULT_TENANT_ID"))
                .unwrap_or_else(|| DEFAULT_TENANT_ID.to_owned()),
            dls_enabled: env_bool("DLS_ENABLED").unwrap_or(false),
            http_log: env_bool("US_RS_HTTP_LOG").unwrap_or(true),
            read_only_mode: env_bool("READ_ONLY_MODE").unwrap_or(false),
            master_tokens: env_variable("MASTER_TOKEN")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|token| !token.is_empty())
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            dynamic_master_keys_ui: ["PRIMARY", "SECONDARY"]
                .iter()
                .filter_map(|suffix| env_variable(&format!("UI_MASTER_TOKEN_PUBLIC_KEY_{suffix}")))
                .map(|key| unescape_pem(&key))
                .collect(),
            // Feature flags of the open-source config; env overrides keep the
            // container configurable without a rebuild.
            dynamic_master_token_enabled: env_bool("DYNAMIC_MASTER_TOKEN_ENABLED").unwrap_or(true),
            dynamic_master_token_is_required: env_bool("DYNAMIC_MASTER_TOKEN_IS_REQUIRED")
                .unwrap_or(false),
            request_timeout_ms: env_u64("US_RS_REQUEST_TIMEOUT_MS").unwrap_or(0),
            trust_proxy: env_u64("TRUST_PROXY")
                .map(|value| value.min(16) as u32)
                .unwrap_or(3),
            dynamic_master_keys_bi: ["PRIMARY", "SECONDARY"]
                .iter()
                .filter_map(|suffix| env_variable(&format!("BI_MASTER_TOKEN_PUBLIC_KEY_{suffix}")))
                .map(|key| unescape_pem(&key))
                .collect(),
        }
    }
}
