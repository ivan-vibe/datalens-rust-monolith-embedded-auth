//! Durable job store for workbook export/import transfers.
//!
//! The Node meta-manager ran both transfers as Temporal workflows and kept their state in
//! `pg-meta-manager-db`. This module keeps the same observable job API the HTTP routes use
//! (start / status / result / cancel) and keeps the process-local `Mutex<HashMap>` as a
//! write-through cache, so reads stay synchronous while the state lives in PostgreSQL.
//!
//! Durability contract:
//! * `start_export` / `start_import` persist the job row before returning the job id;
//! * terminal status / result / error / notifications are persisted before a job is reported
//!   as done (the transfer task awaits the write);
//! * progress is best-effort: a failed write only affects the progress shown for a job that a
//!   restart will mark interrupted anyway;
//! * on boot every `pending` row is marked `error` with `interrupted by restart` and the full
//!   table is loaded back into memory, so the status/result routes keep working after a
//!   monolith restart without any auth headers;
//! * with no PostgreSQL configured or reachable the store logs once and falls back to
//!   memory-only, so bench/rail and dev still start.
//!
//! DSN: `META_MANAGER_PG_DSN` (recommended: `.../pg-meta-manager-db`, the database the Node
//! meta-manager used), else the first entry of `POSTGRES_DSN_LIST` (already present in the
//! monolith; the table then lives next to the US data). `sslmode=disable` is appended when
//! the DSN does not name a TLS mode: this store uses `tokio_postgres::NoTls`, and an explicit
//! `sslmode=require` is treated as unavailable and falls back to memory.

use crate::meta_manager::{
    EntryNotification, ExportPayload, MetaManager, Notification, TransferError,
    EXPORT_DATA_VERSION, NOTIF_EXPORT_CANCELLED, NOTIF_UNEXPECTED_ERROR,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const JOB_TABLE: &str = "workbook_transfer_jobs";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS workbook_transfer_jobs (
    job_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    workbook_id TEXT NOT NULL,
    status TEXT NOT NULL,
    progress INT NOT NULL DEFAULT 0,
    notifications JSONB NOT NULL DEFAULT '[]'::jsonb,
    result JSONB,
    error TEXT,
    cancelled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS workbook_transfer_jobs_updated_at_idx
    ON workbook_transfer_jobs (updated_at DESC);";

const UPSERT_SQL: &str = "\
INSERT INTO workbook_transfer_jobs
    (job_id, kind, workbook_id, status, progress, notifications, result, error, cancelled)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
ON CONFLICT (job_id) DO UPDATE SET
    kind = EXCLUDED.kind,
    workbook_id = EXCLUDED.workbook_id,
    status = EXCLUDED.status,
    progress = EXCLUDED.progress,
    notifications = EXCLUDED.notifications,
    result = EXCLUDED.result,
    error = EXCLUDED.error,
    cancelled = EXCLUDED.cancelled,
    updated_at = now()";

const UPDATE_PROGRESS_SQL: &str = "\
UPDATE workbook_transfer_jobs SET progress = $2, updated_at = now() WHERE job_id = $1";

const UPDATE_CANCELLED_SQL: &str = "\
UPDATE workbook_transfer_jobs SET cancelled = TRUE, updated_at = now() WHERE job_id = $1";

/// A job still `pending` when the process starts can never finish: its transfer task is gone.
/// Mark it failed and attach the same critical notification the Node workflow produced for an
/// unexpected error, so the client stops polling instead of waiting forever.
const INTERRUPT_SQL: &str = "\
UPDATE workbook_transfer_jobs
SET status = 'error',
    error = 'interrupted by restart',
    notifications = COALESCE(notifications, '[]'::jsonb) || jsonb_build_array(
        jsonb_build_object(
            'code', 'UNEXPECTED_WORKFLOW_ERROR',
            'level', 'critical',
            'message', 'Job was interrupted by a monolith restart.'
        )
    ),
    updated_at = now()
WHERE status = 'pending'";

/// The Node meta-manager expired exports and imports after one day
/// (`WORKBOOK_EXPORT_EXPIRATION_DAYS` / `WORKBOOK_IMPORT_EXPIRATION_DAYS`). Recovered jobs are
/// loaded into memory, so keep the same window instead of growing the cache forever.
const PURGE_SQL: &str = "\
DELETE FROM workbook_transfer_jobs
WHERE updated_at < now() - INTERVAL '1 day'";

const LOAD_SQL: &str = "\
SELECT job_id, kind, workbook_id, status, progress, notifications, result, error
FROM workbook_transfer_jobs
ORDER BY updated_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Export,
    Import,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Import => "import",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "export" => Some(Self::Export),
            "import" => Some(Self::Import),
            _ => None,
        }
    }
}

/// Node statuses are exactly these three: a running job stays `Pending` with progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Success,
    Error,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Error => "error",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "success" => Some(Self::Success),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// What a status route needs; `job_id` is the opaque `exportId`/`importId` token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatusView {
    pub job_id: String,
    pub workbook_id: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub progress: u8,
    pub notifications: Vec<EntryNotification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Job {
    kind: JobKind,
    workbook_id: String,
    status: JobStatus,
    progress: u8,
    notifications: Vec<EntryNotification>,
    result: Option<Arc<Value>>,
    error: Option<String>,
    cancel: Arc<AtomicBool>,
}

/// Durable representation of a job, one per `workbook_transfer_jobs` row. Keeping the SQL
/// row out of the conversion means the mapping is unit-testable without a database.
struct JobRow {
    job_id: String,
    kind: JobKind,
    workbook_id: String,
    status: JobStatus,
    progress: u8,
    notifications: Value,
    result: Option<Value>,
    error: Option<String>,
    cancelled: bool,
}

impl JobRow {
    fn from_memory(job_id: &str, job: &Job) -> Self {
        Self {
            job_id: job_id.to_owned(),
            kind: job.kind,
            workbook_id: job.workbook_id.clone(),
            status: job.status,
            progress: job.progress,
            notifications: serde_json::to_value(&job.notifications).unwrap_or_else(|_| json!([])),
            result: job.result.as_ref().map(|result| result.as_ref().clone()),
            error: job.error.clone(),
            cancelled: job.cancel.load(Ordering::Relaxed),
        }
    }

    fn into_memory(self) -> (String, Job) {
        let notifications = serde_json::from_value(self.notifications).unwrap_or_default();
        (
            self.job_id,
            Job {
                kind: self.kind,
                workbook_id: self.workbook_id,
                status: self.status,
                progress: self.progress,
                notifications,
                result: self.result.map(Arc::new),
                error: self.error,
                // A recovered job is terminal, so the flag is never read instead of the DB.
                cancel: Arc::new(AtomicBool::new(false)),
            },
        )
    }
}

/// Where to persist jobs. `dsn: None` keeps the store in memory (tests, bench/rail, dev).
#[derive(Debug, Clone, Default)]
pub struct QueueConfig {
    pub dsn: Option<String>,
}

impl QueueConfig {
    pub fn from_env() -> Self {
        let explicit = std::env::var("META_MANAGER_PG_DSN").ok();
        Self {
            dsn: explicit
                .map(|dsn| dsn.trim().to_owned())
                .filter(|dsn| !dsn.is_empty())
                .or_else(|| first_dsn(std::env::var("POSTGRES_DSN_LIST").ok().as_deref())),
        }
    }
}

fn first_dsn(list: Option<&str>) -> Option<String> {
    list?
        .split(',')
        .map(str::trim)
        .find(|dsn| !dsn.is_empty())
        .map(str::to_owned)
}

/// PostgreSQL side of the store: one connection (its driver task is spawned by `connect`).
/// `Client` is `Send + Sync` and pipelines concurrent queries internally, so no mutex is
/// needed around it.
struct PgJobs {
    client: Arc<tokio_postgres::Client>,
}

impl PgJobs {
    async fn connect(dsn: &str) -> Result<Self, String> {
        let config: tokio_postgres::Config = normalize_dsn(dsn)
            .parse()
            .map_err(|error| format!("invalid DSN: {error}"))?;
        let (client, connection) =
            tokio::time::timeout(CONNECT_TIMEOUT, config.connect(tokio_postgres::NoTls))
                .await
                .map_err(|_| format!("connect timed out after {CONNECT_TIMEOUT:?}"))?
                .map_err(describe)?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!(%error, "workbook job store PostgreSQL connection closed; continuing from memory cache");
            }
        });
        let store = Self {
            client: Arc::new(client),
        };
        store
            .client
            .batch_execute(SCHEMA_SQL)
            .await
            .map_err(describe)?;
        Ok(store)
    }

    async fn persist(&self, row: &JobRow) -> Result<(), String> {
        self.client
            .execute(
                UPSERT_SQL,
                &[
                    &row.job_id,
                    &row.kind.as_str(),
                    &row.workbook_id,
                    &row.status.as_str(),
                    &i32::from(row.progress),
                    &row.notifications,
                    &row.result,
                    &row.error,
                    &row.cancelled,
                ],
            )
            .await
            .map(|_| ())
            .map_err(describe)
    }

    async fn set_progress(&self, job_id: &str, progress: u8) -> Result<(), String> {
        self.client
            .execute(UPDATE_PROGRESS_SQL, &[&job_id, &i32::from(progress)])
            .await
            .map(|_| ())
            .map_err(describe)
    }

    async fn set_cancelled(&self, job_id: &str) -> Result<(), String> {
        self.client
            .execute(UPDATE_CANCELLED_SQL, &[&job_id])
            .await
            .map(|_| ())
            .map_err(describe)
    }

    /// Purge jobs past the Node expiry window, mark stale `pending` jobs as interrupted, then
    /// load the remaining jobs back into memory.
    async fn recover(&self) -> Result<Vec<JobRow>, String> {
        let purged = self
            .client
            .execute(PURGE_SQL, &[])
            .await
            .map_err(describe)?;
        if purged > 0 {
            tracing::info!(
                jobs = purged,
                "purged expired workbook transfer jobs (1 day retention)"
            );
        }
        let interrupted = self
            .client
            .execute(INTERRUPT_SQL, &[])
            .await
            .map_err(describe)?;
        if interrupted > 0 {
            tracing::warn!(
                jobs = interrupted,
                "marked stale workbook transfer jobs as interrupted by restart"
            );
        }
        let rows = self.client.query(LOAD_SQL, &[]).await.map_err(describe)?;
        rows.iter().map(row_to_job_row).collect()
    }
}

fn normalize_dsn(dsn: &str) -> String {
    if dsn.contains("sslmode=") || !dsn.contains("://") {
        return dsn.to_owned();
    }
    let separator = if dsn.contains('?') { '&' } else { '?' };
    format!("{dsn}{separator}sslmode=disable")
}

fn describe(error: tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        ),
        None => error.to_string(),
    }
}

fn row_to_job_row(row: &tokio_postgres::Row) -> Result<JobRow, String> {
    let kind: String = row.try_get("kind").map_err(describe)?;
    let status: String = row.try_get("status").map_err(describe)?;
    let progress: i32 = row.try_get("progress").map_err(describe)?;
    Ok(JobRow {
        job_id: row.try_get("job_id").map_err(describe)?,
        kind: JobKind::parse(&kind)
            .ok_or_else(|| format!("unknown job kind in workbook_transfer_jobs: {kind}"))?,
        workbook_id: row.try_get("workbook_id").map_err(describe)?,
        status: JobStatus::parse(&status)
            .ok_or_else(|| format!("unknown job status in workbook_transfer_jobs: {status}"))?,
        progress: progress.clamp(0, 100) as u8,
        notifications: row.try_get("notifications").map_err(describe)?,
        result: row.try_get("result").map_err(describe)?,
        error: row.try_get("error").map_err(describe)?,
        cancelled: false,
    })
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<HashMap<String, Job>>,
    next_id: AtomicU64,
    pg: Option<Arc<PgJobs>>,
}

/// Cloneable handle shared by the HTTP handlers and the spawned transfer tasks.
#[derive(Clone, Default)]
pub struct JobStore {
    inner: Arc<Inner>,
}

impl JobStore {
    /// Memory-only store (tests, bench/rail, and the fallback when PostgreSQL is unavailable).
    pub fn new() -> Self {
        Self::default()
    }

    /// Durable store from `META_MANAGER_PG_DSN` / `POSTGRES_DSN_LIST`, memory-only otherwise.
    pub async fn init_from_env() -> Self {
        Self::init(QueueConfig::from_env()).await
    }

    /// Connect, create the table if needed, and load/recover existing jobs. The returned store
    /// is always usable: a connection or schema failure logs once and yields the in-memory
    /// fallback instead of failing startup.
    pub async fn init(config: QueueConfig) -> Self {
        let Some(dsn) = config.dsn else {
            tracing::info!(
                "META_MANAGER_PG_DSN/POSTGRES_DSN_LIST unset: workbook transfer jobs are in-memory only"
            );
            return Self::default();
        };
        let pg = match PgJobs::connect(&dsn).await {
            Ok(pg) => Arc::new(pg),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "workbook job store PostgreSQL unavailable; falling back to in-memory jobs"
                );
                return Self::default();
            }
        };
        let rows = match pg.recover().await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "workbook job store recovery failed; falling back to in-memory jobs"
                );
                return Self::default();
            }
        };
        let mut jobs = HashMap::with_capacity(rows.len());
        for row in rows {
            let (job_id, job) = row.into_memory();
            jobs.insert(job_id, job);
        }
        tracing::info!(
            jobs = jobs.len(),
            "workbook transfer job store attached to PostgreSQL"
        );
        Self {
            inner: Arc::new(Inner {
                jobs: Mutex::new(jobs),
                next_id: AtomicU64::new(0),
                pg: Some(pg),
            }),
        }
    }

    /// `false` when the store runs from memory only (status/result then die with the process).
    pub fn is_durable(&self) -> bool {
        self.inner.pg.is_some()
    }

    /// Spawn an export and return its job id (the `exportId` of the status/result routes).
    pub async fn start_export(
        &self,
        client: Arc<MetaManager>,
        workbook_id: impl Into<String>,
    ) -> String {
        let workbook_id = workbook_id.into();
        let (job_id, cancel) = self.insert(JobKind::Export, workbook_id.clone());
        self.persist(&job_id).await;
        let store = self.clone();
        let task_job_id = job_id.clone();
        tokio::spawn(async move {
            let mut notifications = Vec::new();
            let progress_store = store.clone();
            let progress_id = task_job_id.clone();
            let outcome = client
                .export_workbook(&workbook_id, &cancel, &mut notifications, move |progress| {
                    progress_store.set_progress(&progress_id, progress);
                })
                .await;
            match outcome {
                Ok(payload) => match serde_json::to_value(&payload) {
                    Ok(data) => {
                        let result = json!({
                            "exportId": task_job_id,
                            "status": "success",
                            "data": data,
                        });
                        store.complete(&task_job_id, result, notifications).await;
                    }
                    Err(error) => {
                        store
                            .fail(
                                &task_job_id,
                                with_unexpected_notification(
                                    JobKind::Export,
                                    notifications,
                                    Some(error.to_string()),
                                ),
                                Some(error.to_string()),
                            )
                            .await;
                    }
                },
                Err(TransferError::Cancelled) => {
                    notifications.push(critical_notification(
                        NOTIF_EXPORT_CANCELLED,
                        "Workbook export was cancelled.",
                    ));
                    store
                        .fail(
                            &task_job_id,
                            notifications,
                            Some("workbook export was cancelled".into()),
                        )
                        .await;
                }
                Err(TransferError::Failed(error)) => {
                    let notifications = with_unexpected_notification(
                        JobKind::Export,
                        notifications,
                        Some(error.clone()),
                    );
                    store.fail(&task_job_id, notifications, Some(error)).await;
                }
            }
        });
        job_id
    }

    /// Validate an export payload and spawn the import; the job id is the `importId`.
    ///
    /// Version mismatch is rejected synchronously (Node: `WORKBOOK_EXPORT_DATA_OUTDATED`);
    /// a hash mismatch only warns, like Node, so exports produced by the old meta-manager
    /// stay importable.
    pub async fn start_import(
        &self,
        client: Arc<MetaManager>,
        workbook_id: impl Into<String>,
        data: ExportPayload,
    ) -> Result<String, TransferError> {
        let workbook_id = workbook_id.into();
        if data.export.version != EXPORT_DATA_VERSION {
            return Err(TransferError::Failed(format!(
                "workbook export data version {} is outdated",
                data.export.version
            )));
        }
        let expected = client.export_verification_hash(&data.export);
        if data.hash != expected {
            tracing::warn!(
                workbook_id = %workbook_id,
                "workbook import data hash mismatch; continuing"
            );
        }

        let (job_id, cancel) = self.insert(JobKind::Import, workbook_id.clone());
        self.persist(&job_id).await;
        let store = self.clone();
        let task_job_id = job_id.clone();
        tokio::spawn(async move {
            let mut notifications = Vec::new();
            let progress_store = store.clone();
            let progress_id = task_job_id.clone();
            let outcome = client
                .import_workbook(
                    &workbook_id,
                    &data.export,
                    &cancel,
                    &mut notifications,
                    move |progress| progress_store.set_progress(&progress_id, progress),
                )
                .await;
            match outcome {
                Ok(outcome) => {
                    let result = json!({
                        "importId": task_job_id,
                        "workbookId": workbook_id,
                        "status": "success",
                        "idMapping": outcome.id_mapping,
                        "imported": outcome.imported,
                        "skippedScopes": outcome.skipped_scopes,
                    });
                    store.complete(&task_job_id, result, notifications).await;
                }
                Err(TransferError::Cancelled) => {
                    let notifications =
                        with_unexpected_notification(JobKind::Import, notifications, None);
                    store
                        .fail(
                            &task_job_id,
                            notifications,
                            Some("workbook import was cancelled".into()),
                        )
                        .await;
                }
                Err(TransferError::Failed(error)) => {
                    let notifications = with_unexpected_notification(
                        JobKind::Import,
                        notifications,
                        Some(error.clone()),
                    );
                    store.fail(&task_job_id, notifications, Some(error)).await;
                }
            }
        });
        Ok(job_id)
    }

    pub fn status(&self, job_id: &str) -> Option<JobStatusView> {
        let jobs = self.jobs();
        let job = jobs.get(job_id)?;
        Some(JobStatusView {
            job_id: job_id.to_owned(),
            workbook_id: job.workbook_id.clone(),
            kind: job.kind,
            status: job.status,
            progress: job.progress,
            notifications: job.notifications.clone(),
            error: job.error.clone(),
        })
    }

    /// Node-shaped result of a successful transfer, `None` while pending and after an error.
    pub fn result(&self, job_id: &str) -> Option<Arc<Value>> {
        let jobs = self.jobs();
        let job = jobs.get(job_id)?;
        (job.status == JobStatus::Success)
            .then(|| job.result.clone())
            .flatten()
    }

    /// Ask a pending job to stop. It finishes with status `Error` and the cancellation
    /// notification on its next entry boundary. Returns `false` for unknown/finished jobs.
    pub async fn cancel(&self, job_id: &str) -> bool {
        let cancelled = {
            let jobs = self.jobs();
            match jobs.get(job_id) {
                Some(job) if job.status == JobStatus::Pending => {
                    job.cancel.store(true, Ordering::Relaxed);
                    true
                }
                _ => false,
            }
        };
        if cancelled {
            if let Some(pg) = &self.inner.pg {
                if let Err(error) = pg.set_cancelled(job_id).await {
                    tracing::warn!(job_id, %error, "failed to persist job cancellation");
                }
            }
        }
        cancelled
    }

    fn jobs(&self) -> MutexGuard<'_, HashMap<String, Job>> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn next_job_id(&self, kind: JobKind) -> String {
        let sequence = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let prefix = match kind {
            JobKind::Export => 'e',
            JobKind::Import => 'i',
        };
        format!("{prefix}{nanos:x}{sequence:x}")
    }

    fn insert(&self, kind: JobKind, workbook_id: String) -> (String, Arc<AtomicBool>) {
        let job_id = self.next_job_id(kind);
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs().insert(
            job_id.clone(),
            Job {
                kind,
                workbook_id,
                status: JobStatus::Pending,
                progress: 0,
                notifications: Vec::new(),
                result: None,
                error: None,
                cancel: cancel.clone(),
            },
        );
        (job_id, cancel)
    }

    /// Progress is persisted best-effort from the transfer task: it is cosmetic for a job that
    /// a restart will mark interrupted, so the write is detached from the entry loop.
    fn set_progress(&self, job_id: &str, progress: u8) {
        let next = progress.min(100);
        let changed = {
            let mut jobs = self.jobs();
            match jobs.get_mut(job_id) {
                Some(job) if job.status == JobStatus::Pending => {
                    if job.progress == next {
                        false
                    } else {
                        job.progress = next;
                        true
                    }
                }
                _ => false,
            }
        };
        let (Some(pg), true) = (&self.inner.pg, changed) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pg = Arc::clone(pg);
        let job_id = job_id.to_owned();
        handle.spawn(async move {
            if let Err(error) = pg.set_progress(&job_id, next).await {
                tracing::debug!(job_id, %error, "failed to persist job progress");
            }
        });
    }

    async fn complete(&self, job_id: &str, result: Value, notifications: Vec<EntryNotification>) {
        {
            let mut jobs = self.jobs();
            if let Some(job) = jobs.get_mut(job_id) {
                job.status = JobStatus::Success;
                job.progress = 100;
                job.notifications = notifications;
                job.result = Some(Arc::new(result));
                job.error = None;
            }
        }
        self.persist(job_id).await;
    }

    async fn fail(
        &self,
        job_id: &str,
        notifications: Vec<EntryNotification>,
        error: Option<String>,
    ) {
        {
            let mut jobs = self.jobs();
            if let Some(job) = jobs.get_mut(job_id) {
                if job.status == JobStatus::Pending {
                    job.status = JobStatus::Error;
                    job.notifications = notifications;
                    job.error = error;
                }
            }
        }
        self.persist(job_id).await;
    }

    /// Write the current memory state of one job to PostgreSQL. A failed write keeps the job
    /// usable in this process (the HTTP behavior does not degrade) and is logged once per
    /// terminal transition.
    async fn persist(&self, job_id: &str) {
        let Some(pg) = &self.inner.pg else {
            return;
        };
        let row = {
            let jobs = self.jobs();
            let Some(job) = jobs.get(job_id) else {
                return;
            };
            JobRow::from_memory(job_id, job)
        };
        if let Err(error) = pg.persist(&row).await {
            tracing::warn!(job_id, %error, "failed to persist workbook transfer job");
        }
    }
}

fn critical_notification(code: &str, message: &str) -> EntryNotification {
    EntryNotification {
        entry_id: None,
        scope: None,
        notification: Notification {
            code: code.to_owned(),
            level: Some("critical".into()),
            message: Some(message.to_owned()),
            details: None,
        },
    }
}

/// Node adds the generic error notification only for unexpected failures; a run that
/// already reported a critical entry notification keeps that one instead.
fn with_unexpected_notification(
    kind: JobKind,
    mut notifications: Vec<EntryNotification>,
    error: Option<String>,
) -> Vec<EntryNotification> {
    if !notifications
        .iter()
        .any(|entry| entry.notification.is_critical())
    {
        let message = match kind {
            JobKind::Export => "Unexpected error while exporting the workbook.",
            JobKind::Import => "Unexpected error while importing the workbook.",
        };
        let mut notification = critical_notification(NOTIF_UNEXPECTED_ERROR, message);
        notification.notification.details = error.map(|error| json!({ "error": error }));
        notifications.push(notification);
    }
    notifications
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_notification(code: &str) -> EntryNotification {
        EntryNotification {
            entry_id: Some("id".into()),
            scope: Some("widget".into()),
            notification: Notification {
                code: code.into(),
                level: Some("warning".into()),
                message: Some("message".into()),
                details: None,
            },
        }
    }

    fn sample_job() -> Job {
        Job {
            kind: JobKind::Import,
            workbook_id: "wb".into(),
            status: JobStatus::Success,
            progress: 100,
            notifications: vec![entry_notification("NOTIF.WB_IMPORT.CONN.CHECK_CREDENTIALS")],
            result: Some(Arc::new(json!({"importId": "i1", "imported": 96}))),
            error: Some("none".into()),
            cancel: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn job_row_roundtrip_preserves_persisted_fields() {
        let job = sample_job();
        let row = JobRow::from_memory("i1", &job);
        assert_eq!(row.kind, JobKind::Import);
        assert_eq!(row.status, JobStatus::Success);
        assert_eq!(row.progress, 100);
        assert!(row.cancelled);

        let (job_id, restored) = row.into_memory();
        assert_eq!(job_id, "i1");
        assert_eq!(restored.kind, JobKind::Import);
        assert_eq!(restored.workbook_id, "wb");
        assert_eq!(restored.status, JobStatus::Success);
        assert_eq!(restored.progress, 100);
        assert_eq!(restored.notifications, job.notifications);
        assert_eq!(
            restored.result.as_deref(),
            Some(&json!({"importId": "i1", "imported": 96}))
        );
        assert_eq!(restored.error.as_deref(), Some("none"));
    }

    #[test]
    fn kind_and_status_parse_roundtrip() {
        for kind in [JobKind::Export, JobKind::Import] {
            assert_eq!(JobKind::parse(kind.as_str()), Some(kind));
        }
        for status in [JobStatus::Pending, JobStatus::Success, JobStatus::Error] {
            assert_eq!(JobStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(JobKind::parse("dash"), None);
        assert_eq!(JobStatus::parse("running"), None);
    }

    #[test]
    fn first_dsn_takes_the_first_non_empty_entry() {
        assert_eq!(
            first_dsn(Some(" , postgres://a/db, postgres://b/db")),
            Some("postgres://a/db".into())
        );
        assert_eq!(first_dsn(Some("")), None);
        assert_eq!(first_dsn(None), None);
    }

    #[test]
    fn normalize_dsn_forces_no_tls_only_for_urls_without_a_mode() {
        assert_eq!(
            normalize_dsn("postgres://u:p@h:5432/db"),
            "postgres://u:p@h:5432/db?sslmode=disable"
        );
        assert_eq!(
            normalize_dsn("postgres://u:p@h/db?application_name=x"),
            "postgres://u:p@h/db?application_name=x&sslmode=disable"
        );
        assert_eq!(
            normalize_dsn("postgres://u:p@h/db?sslmode=require"),
            "postgres://u:p@h/db?sslmode=require"
        );
        assert_eq!(normalize_dsn("host=h user=u"), "host=h user=u");
    }

    #[tokio::test]
    async fn store_without_a_dsn_is_memory_only() {
        let store = JobStore::init(QueueConfig { dsn: None }).await;
        assert!(!store.is_durable());
        assert!(!JobStore::new().is_durable());
    }

    #[tokio::test]
    async fn new_job_is_pending_with_zero_progress() {
        let store = JobStore::new();
        let (job_id, _cancel) = store.insert(JobKind::Export, "wb".into());

        let view = store.status(&job_id).unwrap();
        assert_eq!(view.status, JobStatus::Pending);
        assert_eq!(view.progress, 0);
        assert_eq!(view.workbook_id, "wb");
        assert_eq!(view.kind, JobKind::Export);
        assert!(store.result(&job_id).is_none());
    }

    #[test]
    fn progress_updates_only_while_pending_and_is_capped() {
        let store = JobStore::new();
        let (job_id, _cancel) = store.insert(JobKind::Import, "wb".into());

        store.set_progress(&job_id, 42);
        assert_eq!(store.status(&job_id).unwrap().progress, 42);
        store.set_progress(&job_id, 250);
        assert_eq!(store.status(&job_id).unwrap().progress, 100);
    }

    #[tokio::test]
    async fn success_sets_result_and_ignores_late_progress() {
        let store = JobStore::new();
        let (job_id, _cancel) = store.insert(JobKind::Export, "wb".into());

        store
            .complete(&job_id, json!({"exportId": "x"}), Vec::new())
            .await;
        store.set_progress(&job_id, 5);

        let view = store.status(&job_id).unwrap();
        assert_eq!(view.status, JobStatus::Success);
        assert_eq!(view.progress, 100);
        assert_eq!(
            store.result(&job_id).unwrap().as_ref(),
            &json!({"exportId": "x"})
        );
        assert!(!store.cancel(&job_id).await);
    }

    #[tokio::test]
    async fn cancel_requests_stop_for_pending_jobs_only() {
        let store = JobStore::new();
        let (pending, cancel) = store.insert(JobKind::Export, "wb".into());
        let (finished, _cancel) = store.insert(JobKind::Export, "wb".into());

        assert!(store.cancel(&pending).await);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!store.cancel("unknown").await);

        store.complete(&finished, json!({}), Vec::new()).await;
        assert!(!store.cancel(&finished).await);
    }

    #[tokio::test]
    async fn failure_keeps_notifications_and_hides_result() {
        let store = JobStore::new();
        let (job_id, _cancel) = store.insert(JobKind::Export, "wb".into());

        let notifications =
            with_unexpected_notification(JobKind::Export, Vec::new(), Some("boom".into()));
        store
            .fail(&job_id, notifications, Some("boom".into()))
            .await;

        let view = store.status(&job_id).unwrap();
        assert_eq!(view.status, JobStatus::Error);
        assert_eq!(view.error.as_deref(), Some("boom"));
        assert_eq!(view.notifications.len(), 1);
        assert_eq!(
            view.notifications[0].notification.code,
            NOTIF_UNEXPECTED_ERROR
        );
        assert!(store.result(&job_id).is_none());
    }

    #[test]
    fn critical_entry_notification_replaces_the_generic_one() {
        let entry = EntryNotification {
            entry_id: Some("id".into()),
            scope: Some("widget".into()),
            notification: Notification {
                code: "ERR.DS_API.WB_EXPORT.DS".into(),
                level: Some("critical".into()),
                message: None,
                details: None,
            },
        };
        let notifications =
            with_unexpected_notification(JobKind::Export, vec![entry.clone()], Some("boom".into()));
        assert_eq!(notifications, vec![entry]);
    }
}
