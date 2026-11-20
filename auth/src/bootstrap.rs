//! First-run schema for `pg-auth-db` in the single-binary fork.
//!
//! There is no separate `datalens-auth` container here, so the monolith has to create the
//! auth schema and the first admin itself. `schema.sql` is the official auth schema (tables,
//! `auth_get_id()` and indexes), schema only — the admin password is hashed in Rust with the
//! same hasher `signin` verifies against.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::password::hash_password;
use crate::validation::ROLE_ADMIN;

/// Credentials of the first user, only used when `auth_users` is empty.
fn admin_credentials() -> (String, String) {
    let login = std::env::var("AUTH_ADMIN_LOGIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "admin".to_string());
    let password = std::env::var("AUTH_ADMIN_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "admin".to_string());
    (login, password)
}

/// Creates the schema when it is absent and the demo admin when there is no user yet.
pub async fn ensure(client: &Client) -> Result<()> {
    let schema_present: bool = client
        .query_one("SELECT to_regclass('public.auth_users') IS NOT NULL", &[])
        .await
        .context("check pg-auth-db schema")?
        .get(0);
    if !schema_present {
        client
            .batch_execute(include_str!("../schema.sql"))
            .await
            .context("create pg-auth-db schema")?;
        eprintln!("auth: created pg-auth-db schema");
    }

    let users: i64 = client
        .query_one("SELECT count(*) FROM auth_users", &[])
        .await
        .context("count auth users")?
        .get(0);
    if users > 0 {
        return Ok(());
    }

    let (login, password) = admin_credentials();
    let hash = hash_password(&password)
        .ok_or_else(|| anyhow::anyhow!("cannot hash the bootstrap admin password"))?;
    let row = client
        .query_one(
            "INSERT INTO auth_users (login, password, first_name, last_name) \
             VALUES ($1, $2, $3, $4) RETURNING user_id",
            &[&login, &hash, &"Admin", &""],
        )
        .await
        .context("insert the bootstrap admin")?;
    let user_id: i64 = row.get("user_id");
    client
        .execute(
            "INSERT INTO auth_roles (user_id, role) VALUES ($1, $2)",
            &[&user_id, &ROLE_ADMIN],
        )
        .await
        .context("grant the bootstrap admin role")?;
    eprintln!("auth: created the bootstrap user {login}");
    Ok(())
}
