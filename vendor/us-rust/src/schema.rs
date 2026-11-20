//! Database schema lifecycle: incremental migrations plus the demo seed.
//!
//! `us-rs` applies the official US 1.196.0 knex migrations instead of creating a
//! dumped schema, so a fresh database and one migrated by an older US version
//! converge on the same schema:
//!
//! ```text
//! us-rs --migrate            # apply pending migrations, exit
//! US_RS_MIGRATE=1 us-rs      # do the same (if needed), then serve
//! ```
//!
//! The normal start path calls [`check_version`] after the optional bootstrap;
//! it now applies the missing migrations instead of rejecting a database whose
//! schema is older than the bundled one.

use anyhow::Context;

use crate::db::Db;
use crate::migrations;

/// Applies pending migrations and, on a fresh database, creates the `common`
/// tenant. Returns `true` when the schema was created or upgraded.
///
/// # Errors
/// Extension installation, a migration statement, or the tenant upsert fails.
pub async fn bootstrap(db: &Db) -> anyhow::Result<bool> {
    let existing: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.entries')::text")
        .fetch_one(&db.primary)
        .await
        .context("checking whether the schema exists")?;

    let applied = migrations::apply_pending(db).await?;

    if existing.is_none() {
        // `20210610180000_add_common_tenant.js` already inserts `common`; the
        // upsert keeps the command idempotent on a hand-restored schema.
        sqlx::query(
            "INSERT INTO public.tenants (tenant_id, enabled, folders_enabled, last_init_at) \
             VALUES ('common', true, true, now()) \
             ON CONFLICT (tenant_id) DO NOTHING",
        )
        .execute(&db.primary)
        .await
        .context("creating the default tenant")?;

        tracing::info!(applied, "database schema created");
        return Ok(true);
    }

    if applied > 0 {
        tracing::info!(applied, "database schema upgraded");
    }

    Ok(applied > 0)
}

/// Startup schema check: applies the missing migrations and then requires the
/// database to be at the last migration of this build.
///
/// # Errors
/// A migration fails, the database is newer than this build, or the `migrations`
/// table is empty.
pub async fn check_version(db: &Db) -> anyhow::Result<()> {
    let applied = migrations::apply_pending(db).await?;
    if applied > 0 {
        tracing::info!(applied, "database schema upgraded");
    }

    let latest = migrations::latest_applied(&db.primary).await?;
    match latest.as_deref() {
        Some(name) if name == migrations::LAST_MIGRATION => Ok(()),
        Some(name) => Err(anyhow::anyhow!(
            "database schema is at migration {name}, this us-rs build expects {} \
             after applying {applied} pending migration(s)",
            migrations::LAST_MIGRATION
        )),
        None => Err(anyhow::anyhow!(
            "the migrations table is empty; create the schema with `us-rs --migrate` \
             or run the official datalens-us image once"
        )),
    }
}

/// Creates a small "Demo" workbook with one empty dashboard. Used for fresh
/// installations (`us-rs --seed-demo`) so the UI is not completely empty.
pub async fn seed_demo(db: &Db) -> anyhow::Result<bool> {
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT workbook_id FROM workbooks \
         WHERE tenant_id = 'common' AND title = 'Demo' AND deleted_at IS NULL \
         LIMIT 1",
    )
    .fetch_optional(&db.primary)
    .await
    .context("checking for an existing demo workbook")?;

    if existing.is_some() {
        return Ok(false);
    }

    let mut tx = db.primary.begin().await.context("begin seed transaction")?;

    let workbook_id: i64 = sqlx::query_scalar(
        "INSERT INTO workbooks \
            (title, tenant_id, created_by, updated_by, title_lower, sort_title) \
         VALUES ('Demo', 'common', 'systemId', 'systemId', lower('Demo'), naturalsort('Demo')) \
         RETURNING workbook_id",
    )
    .fetch_one(&mut *tx)
    .await
    .context("creating demo workbook")?;

    let entry_id: i64 = sqlx::query_scalar(
        "INSERT INTO entries \
            (scope, type, key, name, display_key, created_by, updated_by, tenant_id, \
             workbook_id, unversioned_data) \
         VALUES ('dash', 'dash', $1, 'Demo dashboard', 'Demo dashboard', \
                 'systemId', 'systemId', 'common', $2, '{}'::jsonb) \
         RETURNING entry_id",
    )
    .bind(format!("{workbook_id}/Demo dashboard"))
    .bind(workbook_id)
    .fetch_one(&mut *tx)
    .await
    .context("creating demo dashboard")?;

    let rev_id: i64 = sqlx::query_scalar(
        "INSERT INTO revisions \
            (entry_id, data, meta, created_by, updated_by, version, source_version) \
         VALUES ($1, '{\"blocks\":[],\"tabs\":[]}'::jsonb, '{}'::jsonb, \
                 'systemId', 'systemId', 1, 1) \
         RETURNING rev_id",
    )
    .bind(entry_id)
    .fetch_one(&mut *tx)
    .await
    .context("creating demo revision")?;

    sqlx::query("UPDATE entries SET saved_id = $1, published_id = $1 WHERE entry_id = $2")
        .bind(rev_id)
        .bind(entry_id)
        .execute(&mut *tx)
        .await
        .context("publishing demo dashboard")?;

    tx.commit().await.context("commit demo seed")?;

    Ok(true)
}
