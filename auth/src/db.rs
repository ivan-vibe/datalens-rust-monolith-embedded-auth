//! PostgreSQL access: pooled `tokio-postgres` clients and the auth tables' queries.
//!
//! Everything the Node service does in a transaction stays in one transaction here:
//! session + refresh-token creation, refresh-token rotation + session IP update, and
//! signup (user + role + session + token). Ids come from the database defaults
//! (`auth_get_id()`), so the sequence stays shared with the Node service while both run.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls, Row, Transaction};

use crate::config::Config;
use crate::ids::{decode_id, encode_id, encode_id_for_message};
use crate::jwt::{Jwt, RefreshClaims};
use crate::models::UserData;
use crate::validation::{IdpType, ListQuery, ProfileInput, UpdateField};

pub struct Db {
    dsn: String,
    max_idle: usize,
    idle: Mutex<Vec<Client>>,
}

pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub access_exp: i64,
}

impl Db {
    pub async fn new(dsn: String, max_idle: usize) -> Result<Self> {
        let db = Self {
            dsn,
            max_idle: max_idle.max(1),
            idle: Mutex::new(Vec::new()),
        };
        let client = db
            .connect()
            .await
            .context("connect to pg-auth-db (check AUTH_POSTGRES_DSN)")?;
        crate::bootstrap::ensure(&client)
            .await
            .context("bootstrap pg-auth-db")?;
        db.idle.lock().await.push(client);
        Ok(db)
    }

    async fn connect(&self) -> Result<Client> {
        let (client, connection) = tokio_postgres::connect(&self.dsn, NoTls)
            .await
            .context("open pg-auth-db connection")?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!(%error, "pg-auth-db connection closed with an error");
            }
        });
        Ok(client)
    }

    pub async fn acquire(&self) -> Result<Client> {
        loop {
            let Some(client) = self.idle.lock().await.pop() else {
                return self.connect().await;
            };
            // The shared Postgres can be restarted independently of auth-rs. A pooled client
            // whose driver task saw the socket close must be discarded, not handed to a request.
            if !client.is_closed() {
                return Ok(client);
            }
        }
    }

    pub async fn release(&self, client: Client) {
        if client.is_closed() {
            return;
        }
        let mut idle = self.idle.lock().await;
        if idle.len() < self.max_idle {
            idle.push(client);
        }
    }
}

/// `SELECT user_id, password FROM auth_users WHERE login = $1 AND idp_type IS NULL`.
pub async fn find_login_user(client: &Client, login: &str) -> Result<Option<(i64, Option<String>)>> {
    let row = client
        .query_opt(
            "SELECT user_id, password FROM auth_users \
             WHERE login = $1 AND idp_type IS NULL LIMIT 1",
            &[&login],
        )
        .await?;
    Ok(row.map(|row| (row.get::<_, i64>("user_id"), row.get::<_, Option<String>>("password"))))
}

async fn session_roles(tx: &Transaction<'_>, user_id: i64) -> Result<Vec<String>> {
    let rows = tx
        .query("SELECT role FROM auth_roles WHERE user_id = $1", &[&user_id])
        .await?;
    Ok(rows.iter().map(|row| row.get::<_, String>("role")).collect())
}

/// Insert a session + refresh token and sign the token pair (Node `JwtAuth.startSession`).
/// Runs inside the caller's transaction.
async fn insert_session(
    tx: &Transaction<'_>,
    jwt: &Jwt,
    config: &Config,
    user_id: i64,
    user_agent: &str,
    user_ip: Option<&str>,
) -> Result<Tokens> {
    let row = tx
        .query_one(
            "INSERT INTO auth_sessions (user_id, user_agent, user_ip, expired_at) \
             VALUES ($1, $2, $3, NOW() + ($4::bigint * INTERVAL '1 second')) \
             RETURNING session_id",
            &[&user_id, &user_agent, &user_ip, &config.session_ttl],
        )
        .await?;
    let session_id: i64 = row.get("session_id");

    let roles = session_roles(tx, user_id).await?;
    let user_id_encoded = encode_id(user_id);
    let session_id_encoded = encode_id(session_id);
    let (access, access_exp) = jwt.sign_access(&user_id_encoded, &session_id_encoded, roles)?;

    let row = tx
        .query_one(
            "INSERT INTO auth_refresh_tokens (session_id, expired_at) \
             VALUES ($1, NOW() + ($2::bigint * INTERVAL '1 second')) \
             RETURNING refresh_token_id",
            &[&session_id, &config.refresh_ttl],
        )
        .await?;
    let refresh_token_id: i64 = row.get("refresh_token_id");
    let refresh = jwt.sign_refresh(
        &encode_id(refresh_token_id),
        &user_id_encoded,
        &session_id_encoded,
    )?;

    Ok(Tokens {
        access,
        refresh,
        access_exp,
    })
}

/// `/signin` session creation.
pub async fn signin_tokens(
    client: &mut Client,
    jwt: &Jwt,
    config: &Config,
    user_id: i64,
    user_agent: &str,
    user_ip: Option<&str>,
) -> Result<Tokens> {
    let tx = client.transaction().await?;
    let tokens = insert_session(&tx, jwt, config, user_id, user_agent, user_ip).await?;
    tx.commit().await?;
    Ok(tokens)
}

#[derive(Debug)]
pub enum SignupError {
    /// Pre-check found a local user with the same (case-insensitive) login.
    Exists,
    /// Unique index fired between the pre-check and the insert (Node: 400 DB error).
    UniqueViolation,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for SignupError {
    fn from(error: anyhow::Error) -> Self {
        SignupError::Other(error)
    }
}

fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .map(|db| db.code() == &tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
        .unwrap_or(false)
}

/// Everything `/signup` writes for one new local user.
pub struct SignupInput<'a> {
    pub login: &'a str,
    pub password_hash: &'a str,
    pub email: Option<&'a str>,
    pub first_name: Option<&'a str>,
    pub last_name: Option<&'a str>,
    pub user_agent: &'a str,
    pub user_ip: Option<&'a str>,
}

/// `/signup`: user + default role + session + tokens in one transaction.
pub async fn signup(
    client: &mut Client,
    jwt: &Jwt,
    config: &Config,
    input: &SignupInput<'_>,
) -> Result<Tokens, SignupError> {
    let SignupInput {
        login,
        password_hash,
        email,
        first_name,
        last_name,
        user_agent,
        user_ip,
    } = *input;
    let existing = client
        .query_opt(
            "SELECT 1 FROM auth_users WHERE idp_type IS NULL AND LOWER(login) = LOWER($1) LIMIT 1",
            &[&login],
        )
        .await
        .map_err(|error| SignupError::Other(error.into()))?;
    if existing.is_some() {
        return Err(SignupError::Exists);
    }

    let tx = client.transaction().await.map_err(|error| SignupError::Other(error.into()))?;
    let row = tx
        .query_one(
            "INSERT INTO auth_users (login, password, email, first_name, last_name) \
             VALUES ($1, $2, $3, $4, $5) RETURNING user_id",
            &[&login, &password_hash, &email, &first_name, &last_name],
        )
        .await;
    let row = match row {
        Ok(row) => row,
        Err(error) if is_unique_violation(&error) => return Err(SignupError::UniqueViolation),
        Err(error) => return Err(SignupError::Other(error.into())),
    };
    let user_id: i64 = row.get("user_id");

    if !config.default_role.is_empty() {
        tx.execute(
            "INSERT INTO auth_roles (user_id, role) VALUES ($1, $2)",
            &[&user_id, &config.default_role],
        )
        .await
        .map_err(|error| SignupError::Other(error.into()))?;
    }

    let tokens = insert_session(&tx, jwt, config, user_id, user_agent, user_ip)
        .await
        .map_err(SignupError::Other)?;
    tx.commit()
        .await
        .map_err(|error| SignupError::Other(error.into()))?;
    Ok(tokens)
}

/// Node `JwtAuth.refreshTokens`: validate the DB row, rotate the refresh token, refresh the
/// access token and update the session IP — in one transaction.
pub async fn refresh_tokens(
    client: &mut Client,
    jwt: &Jwt,
    config: &Config,
    claims: &RefreshClaims,
    user_ip: Option<&str>,
) -> Result<Tokens> {
    let Some(user_id) = decode_id(&claims.user_id) else {
        anyhow::bail!("refresh token has an undecodable userId");
    };
    let Some(session_id) = decode_id(&claims.session_id) else {
        anyhow::bail!("refresh token has an undecodable sessionId");
    };
    let Some(refresh_token_id) = decode_id(&claims.refresh_token_id) else {
        anyhow::bail!("refresh token has an undecodable refreshTokenId");
    };

    let row = client
        .query_opt(
            "SELECT s.session_id, s.user_id, s.user_ip, \
                    (s.expired_at <= NOW()) AS session_expired, \
                    rt.refresh_token_id, \
                    (rt.expired_at <= NOW()) AS token_expired \
             FROM auth_sessions s \
             LEFT JOIN auth_refresh_tokens rt ON rt.session_id = s.session_id \
             WHERE s.session_id = $1 AND s.user_id = $2 LIMIT 1",
            &[&session_id, &user_id],
        )
        .await?;
    let Some(row) = row else {
        anyhow::bail!("unknown session");
    };

    let stored_token_id: Option<i64> = row.get("refresh_token_id");
    if stored_token_id != Some(refresh_token_id) {
        // A stale/unknown refresh token for a known session: if it comes from another IP,
        // treat the session as compromised and delete it (Node behaviour).
        let stored_ip: Option<String> = row.get("user_ip");
        if stored_ip.is_some() && stored_ip.as_deref() != user_ip {
            client
                .execute(
                    "DELETE FROM auth_sessions WHERE session_id = $1",
                    &[&session_id],
                )
                .await?;
        }
        anyhow::bail!("unknown refresh token");
    }
    if row.get::<_, Option<bool>>("token_expired") != Some(false) {
        anyhow::bail!("expired refresh token");
    }
    if row.get::<_, bool>("session_expired") {
        anyhow::bail!("expired session");
    }

    let tx = client.transaction().await?;
    tx.execute(
        "DELETE FROM auth_refresh_tokens WHERE refresh_token_id = $1",
        &[&refresh_token_id],
    )
    .await?;

    let roles = session_roles(&tx, user_id).await?;
    let user_id_encoded = encode_id(user_id);
    let session_id_encoded = encode_id(session_id);
    let (access, access_exp) = jwt.sign_access(&user_id_encoded, &session_id_encoded, roles)?;

    let row = tx
        .query_one(
            "INSERT INTO auth_refresh_tokens (session_id, expired_at) \
             VALUES ($1, NOW() + ($2::bigint * INTERVAL '1 second')) \
             RETURNING refresh_token_id",
            &[&session_id, &config.refresh_ttl],
        )
        .await?;
    let new_refresh_token_id: i64 = row.get("refresh_token_id");
    let refresh = jwt.sign_refresh(
        &encode_id(new_refresh_token_id),
        &user_id_encoded,
        &session_id_encoded,
    )?;

    tx.execute(
        "UPDATE auth_sessions SET user_ip = $1 WHERE session_id = $2 AND user_id = $3",
        &[&user_ip, &session_id, &user_id],
    )
    .await?;
    tx.commit().await?;

    Ok(Tokens {
        access,
        refresh,
        access_exp,
    })
}

/// Node `JwtAuth.closeSession`: delete the session only when the presented refresh token is
/// the current one (a rotated/stale token must not let the caller log the session out).
pub async fn close_session(client: &Client, claims: &RefreshClaims) -> Result<()> {
    let Some(refresh_token_id) = decode_id(&claims.refresh_token_id) else {
        anyhow::bail!("refresh token has an undecodable refreshTokenId");
    };
    client
        .execute(
            "DELETE FROM auth_sessions WHERE session_id = \
             (SELECT session_id FROM auth_refresh_tokens WHERE refresh_token_id = $1)",
            &[&refresh_token_id],
        )
        .await?;
    Ok(())
}


pub async fn ping(client: &Client) -> Result<()> {
    client.query_one("SELECT 1 + 1", &[]).await?;
    Ok(())
}

/// Node `getUserProfile`: user row + all roles (left join, roles collected across rows).
pub async fn get_profile(client: &Client, user_id: i64) -> Result<Option<UserData>> {
    let rows: Vec<Row> = client
        .query(
            "SELECT u.user_id, u.login, u.email, u.first_name, u.last_name, u.idp_type, \
                    u.idp_slug, r.role \
             FROM auth_users u LEFT JOIN auth_roles r ON r.user_id = u.user_id \
             WHERE u.user_id = $1",
            &[&user_id],
        )
        .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let mut user = user_from_row(first);
    user.roles = rows
        .iter()
        .filter_map(|row| row.get::<_, Option<String>>("role"))
        .collect();
    Ok(Some(user))
}

fn user_from_row(row: &Row) -> UserData {
    UserData {
        user_id: row.get("user_id"),
        login: row.get("login"),
        email: row.get("email"),
        first_name: row.get("first_name"),
        last_name: row.get("last_name"),
        idp_type: row.get("idp_type"),
        idp_slug: row.get("idp_slug"),
        roles: Vec::new(),
    }
}

/// Tokens expiring in the future from `auth_roles`, used for the Manage permission
/// introspection and for `GET /v1/users/list` role filters.
pub async fn user_roles(client: &Client, user_id: i64) -> Result<Vec<String>> {
    let rows = client
        .query("SELECT role FROM auth_roles WHERE user_id = $1", &[&user_id])
        .await?;
    Ok(rows.iter().map(|row| row.get::<_, String>("role")).collect())
}

fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == '%' || ch == '_' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Node `getUsersList`: a subselect picks the page of user ids (filters live there), the outer
/// query returns every role row of those users, grouped in user-id order.
pub async fn list_users(client: &Client, query: &ListQuery) -> Result<(Vec<UserData>, Option<String>)> {
    let filter: Option<String> = query
        .filter_string
        .as_ref()
        .map(|filter| format!("%{}%", escape_like(&filter.to_lowercase())));
    let roles: Option<Vec<String>> = query.roles.clone();
    let idp_type: Option<String> = match &query.idp_type {
        None => None,
        Some(IdpType::Null) => Some("null".to_string()),
        Some(IdpType::Value(value)) => Some(value.clone()),
    };
    let sql = "SELECT u.user_id, u.login, u.email, u.first_name, u.last_name, u.idp_type, \
                      u.idp_slug, r.role \
               FROM auth_users u LEFT JOIN auth_roles r ON r.user_id = u.user_id \
               WHERE u.user_id IN ( \
                   SELECT DISTINCT u2.user_id FROM auth_users u2 \
                   LEFT JOIN auth_roles r2 ON r2.user_id = u2.user_id \
                   WHERE ($1::text IS NULL OR LOWER(u2.login) LIKE $1 ESCAPE '\\' \
                          OR LOWER(u2.email) LIKE $1 ESCAPE '\\' \
                          OR LOWER(u2.first_name) LIKE $1 ESCAPE '\\' \
                          OR LOWER(u2.last_name) LIKE $1 ESCAPE '\\') \
                     AND ($2::text[] IS NULL OR r2.role = ANY($2)) \
                     AND ($3::text IS NULL \
                          OR ($3 = 'null' AND u2.idp_type IS NULL) \
                          OR u2.idp_type = $3) \
                   ORDER BY u2.user_id LIMIT $4 OFFSET $5 \
               ) \
               ORDER BY u.user_id";
    let rows = client
        .query(
            sql,
            &[
                &filter,
                &roles,
                &idp_type,
                &query.page_size,
                &(query.page * query.page_size),
            ],
        )
        .await?;

    let mut users: Vec<UserData> = Vec::new();
    let mut index_by_id: HashMap<i64, usize> = HashMap::new();
    for row in &rows {
        let user_id: i64 = row.get("user_id");
        if let Some(&index) = index_by_id.get(&user_id) {
            if let Some(role) = row.get::<_, Option<String>>("role") {
                users[index].roles.push(role);
            }
        } else {
            let mut user = user_from_row(row);
            if let Some(role) = row.get::<_, Option<String>>("role") {
                user.roles.push(role);
            }
            index_by_id.insert(user_id, users.len());
            users.push(user);
        }
    }
    let next_page_token = (users.len() == query.page_size.max(0) as usize && query.page >= 0)
        .then(|| (query.page + 1).to_string());
    Ok((users, next_page_token))
}

/// Node `getUsersByIds`: result keeps the request order (duplicates included), missing ids are
/// omitted.
pub async fn users_by_ids(client: &Client, ids: &[i64]) -> Result<Vec<UserData>> {
    let mut unique: Vec<i64> = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        if seen.insert(*id) {
            unique.push(*id);
        }
    }
    let rows = client
        .query(
            "SELECT u.user_id, u.login, u.email, u.first_name, u.last_name, u.idp_type, \
                    u.idp_slug, r.role \
             FROM auth_users u LEFT JOIN auth_roles r ON r.user_id = u.user_id \
             WHERE u.user_id = ANY($1)",
            &[&unique],
        )
        .await?;
    let mut by_id: HashMap<i64, UserData> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for row in &rows {
        let user_id: i64 = row.get("user_id");
        if let Some(user) = by_id.get_mut(&user_id) {
            if let Some(role) = row.get::<_, Option<String>>("role") {
                user.roles.push(role);
            }
        } else {
            let mut user = user_from_row(row);
            if let Some(role) = row.get::<_, Option<String>>("role") {
                user.roles.push(role);
            }
            order.push(user_id);
            by_id.insert(user_id, user);
        }
    }
    Ok(ids
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .collect())
}

#[derive(Debug)]
pub enum CreateUserError {
    Exists,
    UniqueViolation,
    Db(anyhow::Error),
}

/// Node `createUser`: duplicate pre-check, then user + roles in one transaction.
pub async fn create_user(
    client: &mut Client,
    login: &str,
    password_hash: &str,
    email: Option<&str>,
    first_name: Option<&str>,
    last_name: Option<&str>,
    roles: &[String],
) -> Result<i64, CreateUserError> {
    let existing = client
        .query_opt(
            "SELECT 1 FROM auth_users WHERE idp_type IS NULL AND LOWER(login) = LOWER($1) LIMIT 1",
            &[&login],
        )
        .await
        .map_err(|error| CreateUserError::Db(error.into()))?;
    if existing.is_some() {
        return Err(CreateUserError::Exists);
    }

    let tx = client
        .transaction()
        .await
        .map_err(|error| CreateUserError::Db(error.into()))?;
    let row = match tx
        .query_one(
            "INSERT INTO auth_users (login, password, email, first_name, last_name) \
             VALUES ($1, $2, $3, $4, $5) RETURNING user_id",
            &[&login, &password_hash, &email, &first_name, &last_name],
        )
        .await
    {
        Ok(row) => row,
        Err(error) if is_unique_violation(&error) => return Err(CreateUserError::UniqueViolation),
        Err(error) => return Err(CreateUserError::Db(error.into())),
    };
    let user_id: i64 = row.get("user_id");

    let mut seen = HashSet::new();
    for role in roles {
        if !seen.insert(role.clone()) {
            continue;
        }
        tx.execute(
            "INSERT INTO auth_roles (user_id, role, updated_at) VALUES ($1, $2, NOW())",
            &[&user_id, role],
        )
        .await
        .map_err(|error| CreateUserError::Db(error.into()))?;
    }
    tx.commit()
        .await
        .map_err(|error| CreateUserError::Db(error.into()))?;
    Ok(user_id)
}

/// `SELECT idp_slug`: outer `None` = user missing, inner `Some` = IdP-managed user.
pub async fn user_idp_slug(client: &Client, user_id: i64) -> Result<Option<Option<String>>> {
    let row = client
        .query_opt("SELECT idp_slug FROM auth_users WHERE user_id = $1", &[&user_id])
        .await?;
    Ok(row.map(|row| row.get::<_, Option<String>>("idp_slug")))
}

pub async fn delete_user(client: &Client, user_id: i64) -> Result<()> {
    client
        .execute("DELETE FROM auth_users WHERE user_id = $1", &[&user_id])
        .await?;
    Ok(())
}

fn update_field_parts(field: &UpdateField) -> (bool, Option<String>) {
    match field {
        UpdateField::Absent => (false, None),
        UpdateField::Null => (true, None),
        UpdateField::Value(value) => (true, Some(value.clone())),
    }
}

/// Node `updateUserProfile` patch: only provided columns (+ `updated_at`) are written.
pub async fn patch_profile(client: &Client, user_id: i64, input: &ProfileInput) -> Result<()> {
    let (email_set, email) = update_field_parts(&input.email);
    let (first_set, first_name) = update_field_parts(&input.first_name);
    let (last_set, last_name) = update_field_parts(&input.last_name);
    client
        .execute(
            "UPDATE auth_users SET \
                 updated_at = NOW(), \
                 email = CASE WHEN $1::boolean THEN $2::text ELSE email END, \
                 first_name = CASE WHEN $3::boolean THEN $4::text ELSE first_name END, \
                 last_name = CASE WHEN $5::boolean THEN $6::text ELSE last_name END \
             WHERE user_id = $7",
            &[
                &email_set,
                &email,
                &first_set,
                &first_name,
                &last_set,
                &last_name,
                &user_id,
            ],
        )
        .await?;
    Ok(())
}

/// Password + IdP marker, the row `updateUserPassword` reads before checking the old password.
pub async fn user_password_row(
    client: &Client,
    user_id: i64,
) -> Result<Option<(Option<String>, Option<String>)>> {
    let row = client
        .query_opt(
            "SELECT password, idp_slug FROM auth_users WHERE user_id = $1",
            &[&user_id],
        )
        .await?;
    Ok(row.map(|row| {
        (
            row.get::<_, Option<String>>("password"),
            row.get::<_, Option<String>>("idp_slug"),
        )
    }))
}

pub async fn set_password(client: &Client, user_id: i64, password_hash: &str) -> Result<()> {
    client
        .execute(
            "UPDATE auth_users SET password = $1, updated_at = NOW() WHERE user_id = $2",
            &[&password_hash, &user_id],
        )
        .await?;
    Ok(())
}

/// Node `addUsersRoles`: upsert on `(user_id, role)`.
pub async fn add_roles(client: &mut Client, deltas: &[(String, i64)]) -> Result<()> {
    let tx = client.transaction().await?;
    for (role, subject_id) in deltas {
        tx.execute(
            "INSERT INTO auth_roles (user_id, role, updated_at) VALUES ($1, $2, NOW()) \
             ON CONFLICT (user_id, role) DO UPDATE SET updated_at = NOW()",
            &[subject_id, role],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Debug)]
pub enum RoleUpdateError {
    NotExists(String),
    NotConsistent(String),
    Db(anyhow::Error),
}

/// Node `updateUsersRoles`: rename a user's role in place (upsert by `role_id`).
pub async fn update_roles(
    client: &mut Client,
    deltas: &[(String, String, i64)],
) -> Result<(), RoleUpdateError> {
    let mut user_ids: Vec<i64> = Vec::new();
    let mut seen = HashSet::new();
    for (_, _, subject_id) in deltas {
        if seen.insert(*subject_id) {
            user_ids.push(*subject_id);
        }
    }
    let rows = client
        .query(
            "SELECT role_id, user_id, role FROM auth_roles WHERE user_id = ANY($1)",
            &[&user_ids],
        )
        .await
        .map_err(|error| RoleUpdateError::Db(error.into()))?;
    let mut roles_by_user: HashMap<i64, HashSet<String>> = HashMap::new();
    let mut role_ids: HashMap<(String, i64), i64> = HashMap::new();
    for row in &rows {
        let role: String = row.get("role");
        let user_id: i64 = row.get("user_id");
        let role_id: i64 = row.get("role_id");
        roles_by_user.entry(user_id).or_default().insert(role.clone());
        role_ids.insert((role, user_id), role_id);
    }

    let mut upserts: Vec<(i64, i64, String)> = Vec::new();
    for (old_role, new_role, subject_id) in deltas {
        let user_roles = roles_by_user.get(subject_id);
        if !matches!(user_roles, Some(roles) if roles.contains(old_role)) {
            return Err(RoleUpdateError::NotExists(format!(
                "The specified old role \"{old_role}\" was not found for user \"{}\"",
                encode_id_for_message(*subject_id)
            )));
        }
        if matches!(user_roles, Some(roles) if roles.contains(new_role)) {
            return Err(RoleUpdateError::NotConsistent(format!(
                "The specified new role \"{new_role}\" already exists for user \"{}\"",
                encode_id_for_message(*subject_id)
            )));
        }
        let Some(role_id) = role_ids.get(&(old_role.clone(), *subject_id)).copied() else {
            return Err(RoleUpdateError::NotExists(
                "AUTH.ROLE_NOT_EXISTS".to_string(),
            ));
        };
        upserts.push((role_id, *subject_id, new_role.clone()));
    }

    let tx = client
        .transaction()
        .await
        .map_err(|error| RoleUpdateError::Db(error.into()))?;
    for (role_id, user_id, role) in upserts {
        tx.execute(
            "INSERT INTO auth_roles (role_id, user_id, role, updated_at) \
             VALUES ($1, $2, $3, NOW()) \
             ON CONFLICT (role_id) DO UPDATE SET \
                 user_id = excluded.user_id, role = excluded.role, updated_at = excluded.updated_at",
            &[&role_id, &user_id, &role],
        )
        .await
        .map_err(|error| RoleUpdateError::Db(error.into()))?;
    }
    tx.commit()
        .await
        .map_err(|error| RoleUpdateError::Db(error.into()))?;
    Ok(())
}

/// Node `removeUsersRoles`: missing roles are ignored.
pub async fn remove_roles(client: &mut Client, deltas: &[(String, i64)]) -> Result<()> {
    let mut user_ids: Vec<i64> = Vec::new();
    let mut seen = HashSet::new();
    for (_, subject_id) in deltas {
        if seen.insert(*subject_id) {
            user_ids.push(*subject_id);
        }
    }
    let rows = client
        .query(
            "SELECT role_id, user_id, role FROM auth_roles WHERE user_id = ANY($1)",
            &[&user_ids],
        )
        .await?;
    let mut role_ids: Vec<i64> = Vec::new();
    for row in &rows {
        let role: String = row.get("role");
        let user_id: i64 = row.get("user_id");
        let role_id: i64 = row.get("role_id");
        if deltas
            .iter()
            .any(|(delta_role, subject)| delta_role == &role && subject == &user_id)
        {
            role_ids.push(role_id);
        }
    }
    if role_ids.is_empty() {
        return Ok(());
    }
    let tx = client.transaction().await?;
    tx.execute("DELETE FROM auth_roles WHERE role_id = ANY($1)", &[&role_ids])
        .await?;
    tx.commit().await?;
    Ok(())
}
