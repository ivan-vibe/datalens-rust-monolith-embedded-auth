//! Incremental knex-compatible migrations of `datalens-us:1.196.0`.
//!
//! The Node image migrates in its preflight (`scripts/preflight.sh`):
//! `db/scripts/extensions.js` installs the Postgres extensions, then
//! `npm run db:migrate` applies every pending file of `dist/server/db/migrations`
//! in name order and records it in the knex `migrations` table
//! (`id, name, batch, migration_time`).
//!
//! Every `up()` of 1.196.0 is a sequence of `knex.raw(<SQL>)` calls (the five
//! `knex.schema` calls live in `down()`), so the port bundles their SQL verbatim
//! in [`MIGRATIONS_JSON`] and runs the same algorithm: create the knex tables,
//! install the extensions, then apply the missing migrations in order, each in
//! its own transaction unless the file declares `transaction: false`.
//!
//! The JSON was extracted from the official image with
//! `research/v91/fixes/us4/evidence/extract_migrations.py`; the `.js` sources and
//! the extracted list are stored next to it in
//! `research/v91/fixes/us4/evidence/`.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context};
use sqlx::{Connection, Executor, PgConnection, PgPool, Postgres};

use crate::db::Db;

/// The `up()` SQL of every 1.196.0 migration, extracted verbatim.
const MIGRATIONS_JSON: &str = include_str!("migrations.json");

/// `max(name)` of the 1.196.0 migration directory.
pub const LAST_MIGRATION: &str = "20260702101409_add_artifact_scope.js";

/// Arbitrary application-wide key for `pg_advisory_lock`: two `us-rs` processes
/// starting against the same database must not apply the same migration twice.
const MIGRATION_LOCK_ID: i64 = 0x7573_7273_5f6d_6967;

/// One knex migration file: its name, whether knex wraps it in a transaction and
/// the ordered `knex.raw()` statements of `up()`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Migration {
    pub name: String,
    pub transactional: bool,
    pub statements: Vec<String>,
}

/// The migrations bundled with this build, in application order.
///
/// # Errors
/// The embedded JSON is generated at build time and must always parse; a
/// failure means the file was edited by hand.
pub fn bundled() -> anyhow::Result<Vec<Migration>> {
    serde_json::from_str(MIGRATIONS_JSON).context("parsing the bundled migrations")
}

/// The pending bundle indices for a database that recorded `applied`.
///
/// This mirrors knex: a recorded migration that is not bundled means the
/// database was migrated by a newer US version. A migration missing between two
/// applied ones is pending and is applied normally (knex appends it with the
/// next batch).
///
/// # Errors
/// An applied migration is not part of this bundle.
pub fn plan(applied: &[String], bundled: &[Migration]) -> anyhow::Result<Vec<usize>> {
    let positions: HashMap<&str, usize> = bundled
        .iter()
        .enumerate()
        .map(|(index, migration)| (migration.name.as_str(), index))
        .collect();

    for name in applied {
        if !positions.contains_key(name.as_str()) {
            bail!(
                "the database has migration {name}, which this build does not bundle: \
                 it was migrated by a newer DataLens US version"
            );
        }
    }

    let applied: HashSet<&str> = applied.iter().map(String::as_str).collect();
    Ok(bundled
        .iter()
        .enumerate()
        .filter(|(_, migration)| !applied.contains(migration.name.as_str()))
        .map(|(index, _)| index)
        .collect())
}

/// Names in the knex `migrations` table, in insertion order.
///
/// # Errors
/// The table is missing (the database was never migrated) or a query fails.
pub async fn applied_names(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM migrations ORDER BY id")
        .fetch_all(pool)
        .await
        .context("reading the knex migrations table")?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// The last migration recorded in the database.
///
/// # Errors
/// The table is missing or a query fails.
pub async fn latest_applied(pool: &PgPool) -> anyhow::Result<Option<String>> {
    // `max(name)`, not the last inserted row: a migration that was missing
    // between applied ones is re-applied later and gets a new id.
    let name: Option<String> = sqlx::query_scalar("SELECT max(name) FROM migrations")
        .fetch_one(pool)
        .await
        .context("reading the latest knex migration")?;
    Ok(name)
}

/// Applies every bundled migration the database has not recorded yet.
///
/// Returns the number of applied migrations; a started database returns 0, so a
/// second call is a no-op. The whole run is serialized with a Postgres advisory
/// lock, and each migration is committed on its own (like knex).
///
/// # Errors
/// Extension/table setup fails, a migration statement fails (the transaction is
/// rolled back and nothing is recorded), or the database is newer than this
/// build ([`plan`]).
pub async fn apply_pending(db: &Db) -> anyhow::Result<usize> {
    let mut connection = db
        .primary
        .acquire()
        .await
        .context("acquiring a migration connection")?;

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_ID)
        .execute(&mut *connection)
        .await
        .context("locking the migration runner")?;

    let result = apply_locked(&mut connection).await;

    // The lock belongs to the session; release it even when a migration failed
    // so the pooled connection is reusable.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_ID)
        .execute(&mut *connection)
        .await;

    result
}

async fn apply_locked(connection: &mut PgConnection) -> anyhow::Result<usize> {
    ensure_objects(connection).await?;

    let bundled = bundled()?;
    let applied: Vec<String> =
        sqlx::query_as::<_, (String,)>("SELECT name FROM migrations ORDER BY id")
            .fetch_all(&mut *connection)
            .await
            .context("reading the knex migrations table")?
            .into_iter()
            .map(|(name,)| name)
            .collect();

    if applied.is_empty() {
        // A schema without bookkeeping was restored by hand (or by the old
        // `--migrate` dump path): re-running the directory would collide with
        // the existing tables, so refuse instead of half-migrating it.
        let schema_exists: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.entries')::text")
                .fetch_one(&mut *connection)
                .await
                .context("checking whether the schema exists")?;
        if schema_exists.is_some() {
            bail!(
                "the database schema exists but the `migrations` table is empty; \
                 restore a dump that includes the knex migration rows"
            );
        }
    }

    let pending = plan(&applied, &bundled)?;
    if pending.is_empty() {
        return Ok(0);
    }

    let batch: i64 =
        sqlx::query_scalar("SELECT (COALESCE(MAX(batch), 0) + 1)::bigint FROM migrations")
            .fetch_one(&mut *connection)
            .await
            .context("computing the knex migration batch")?;

    let mut count = 0;
    for index in pending {
        let migration = &bundled[index];
        tracing::info!(
            migration = %migration.name,
            transaction = migration.transactional,
            batch,
            "applying knex migration"
        );
        apply_migration(connection, migration, batch).await?;
        count += 1;
    }

    Ok(count)
}

async fn apply_migration(
    connection: &mut PgConnection,
    migration: &Migration,
    batch: i64,
) -> anyhow::Result<()> {
    if migration.transactional {
        let mut tx = connection
            .begin()
            .await
            .with_context(|| format!("begin migration {}", migration.name))?;

        for statement in &migration.statements {
            tx.execute(statement.as_str())
                .await
                .with_context(|| format!("migration {} failed", migration.name))?;
        }

        record(&mut *tx, &migration.name, batch).await?;
        tx.commit()
            .await
            .with_context(|| format!("commit migration {}", migration.name))?;
    } else {
        // `transaction: false` exists for `CREATE INDEX CONCURRENTLY` and
        // `ALTER TYPE ... ADD VALUE`, both of which Postgres rejects inside a
        // transaction block.
        for statement in &migration.statements {
            connection
                .execute(statement.as_str())
                .await
                .with_context(|| format!("migration {} failed", migration.name))?;
        }

        record(&mut *connection, &migration.name, batch).await?;
    }

    Ok(())
}

async fn record<'c, E>(executor: E, name: &str, batch: i64) -> anyhow::Result<()>
where
    E: Executor<'c, Database = Postgres>,
{
    sqlx::query("INSERT INTO migrations (name, batch, migration_time) VALUES ($1, $2, now())")
        .bind(name)
        .bind(batch)
        .execute(executor)
        .await
        .with_context(|| format!("recording migration {name}"))?;
    Ok(())
}

/// `db/scripts/extensions.js` plus the knex bookkeeping tables.
async fn ensure_objects(connection: &mut PgConnection) -> anyhow::Result<()> {
    connection
        .execute(
            "CREATE EXTENSION IF NOT EXISTS pg_trgm;\n\
             CREATE EXTENSION IF NOT EXISTS btree_gin;\n\
             CREATE EXTENSION IF NOT EXISTS btree_gist;\n\
             CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";",
        )
        .await
        .context("installing the Postgres extensions")?;

    connection
        .execute(
            "CREATE TABLE IF NOT EXISTS migrations (\n\
                 id serial PRIMARY KEY,\n\
                 name varchar(255),\n\
                 batch integer,\n\
                 migration_time timestamptz\n\
             );\n\
             CREATE TABLE IF NOT EXISTS migrations_lock (\n\
                 \"index\" serial PRIMARY KEY,\n\
                 is_locked integer\n\
             );\n\
             INSERT INTO migrations_lock (\"index\", is_locked) VALUES (1, 0)\n\
             ON CONFLICT DO NOTHING;",
        )
        .await
        .context("creating the knex migrations tables")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("m{index:03}.js")).collect()
    }

    fn migrations(count: usize) -> Vec<Migration> {
        (0..count)
            .map(|index| Migration {
                name: format!("m{index:03}.js"),
                transactional: true,
                statements: vec!["SELECT 1".to_owned()],
            })
            .collect()
    }

    #[test]
    fn bundle_matches_the_196_migration_directory() {
        let bundled = bundled().expect("bundled migrations parse");
        assert_eq!(bundled.len(), 162);
        assert_eq!(bundled[0].name, "20180601125336_create_conf_storage.js");
        assert_eq!(
            bundled.last().map(|m| m.name.as_str()),
            Some(LAST_MIGRATION)
        );
        assert!(bundled.iter().all(|m| !m.statements.is_empty()));
    }

    #[test]
    fn plan_applies_missing_migrations_in_order() {
        let bundled = migrations(5);

        // Fresh database: every migration is pending, in bundle order.
        assert_eq!(plan(&[], &bundled).unwrap(), vec![0, 1, 2, 3, 4]);

        // Up to date: no-op (the idempotency contract).
        let applied = names(5);
        assert!(plan(&applied, &bundled).unwrap().is_empty());

        // One missing in the middle: only that one is pending.
        let mut with_gap = names(5);
        with_gap.remove(2);
        assert_eq!(plan(&with_gap, &bundled).unwrap(), vec![2]);

        // After the gap is applied, knex records it at the end of the table; a
        // second run is a no-op regardless of the row order.
        let mut after = with_gap;
        after.push("m002.js".to_owned());
        assert!(plan(&after, &bundled).unwrap().is_empty());
    }

    #[test]
    fn plan_rejects_a_newer_or_corrupt_database() {
        let bundled = migrations(3);

        // A migration recorded by a newer US version is not in this build.
        let error = plan(&names(4), &bundled).expect_err("newer migration");
        assert!(error.to_string().contains("does not bundle"), "{error}");
    }
}
