//! Workbook export/import orchestration against the v251 monolith internal routes.
//!
//! This is the reusable core of the Rust meta-manager replacement: it lists a workbook
//! through the in-process US router (`/private/v2/workbooks/{id}/entries`), asks the
//! in-process ui-api transfer routes (`/api/internal/v1/workbooks/{export,import}`) about
//! each entry in scope-dependency order, and assembles the same `data.export` payload the
//! Node meta-manager stores (`{version, entries}` plus its HMAC verification hash).
//!
//! Not ported on purpose: Temporal (retries/queues/history), the meta-manager Postgres
//! `exports`/`imports` tables, workbook creation/access checks, and the final workbook
//! status transition. The in-memory job store in [`crate::workbook_queue`] stands in for
//! the durable part; the HTTP handler owns access control and workbook creation.
//!
//! Wire shapes (verified against `datalens-meta-manager:0.52.0` services/workflows and a
//! live export of workbook `z4wtz6tg5194o`):
//!
//! * ui-api export returns `{notifications, entryData: {"<scope>": {...}}}`;
//! * export result data is `{"export": {"version": "v1", "entries": {scope: {mockId: entryData}}}, "hash": hex}`;
//! * ui-api import takes `{idMapping, workbookId, entryData}` and returns `{id, notifications}`;
//! * the import id mapping is `mockEntryId -> new entryId` and later entries see the accumulated map.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Version stored in `data.export.version`; the Node meta-manager rejects anything else.
pub const EXPORT_DATA_VERSION: &str = "v1";

/// Status notification codes from the Node meta-manager.
pub const NOTIF_EXPORT_CANCELLED: &str = "WORKBOOK_EXPORT_CANCELLED";
pub const NOTIF_UNEXPECTED_ERROR: &str = "UNEXPECTED_WORKFLOW_ERROR";
pub const NOTIF_SCOPE_NOT_AVAILABLE: &str = "SCOPE_NOT_AVAILABLE_FOR_INSTALLATION";

/// Transport/orchestration failure. `Cancelled` is the only failure a caller must tell
/// apart: it maps to the "export cancelled" notification instead of "unexpected error".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    Cancelled,
    Failed(String),
}

pub type TransferResult<T> = Result<T, TransferError>;

impl From<String> for TransferError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<reqwest::Error> for TransferError {
    fn from(error: reqwest::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<serde_json::Error> for TransferError {
    fn from(error: serde_json::Error) -> Self {
        Self::Failed(format!("invalid JSON: {error}"))
    }
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TransferError {}

/// One entry of a workbook as returned by the US entries route. Only the two fields the
/// transfer needs are kept; `entryId` is opaque (encoded US id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRef {
    #[serde(rename = "entryId")]
    pub entry_id: String,
    pub scope: String,
}

/// An entry notification in the shape of the Node `entryNotificationSchema`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryNotification {
    #[serde(rename = "entryId", default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(flatten)]
    pub notification: Notification,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Notification {
    /// Node checks `level === "critical"` and aborts the transfer on the first one.
    pub fn is_critical(&self) -> bool {
        self.level.as_deref() == Some("critical")
    }
}

/// `data.export` payload: the entry data grouped by scope and mock entry id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportData {
    pub version: String,
    pub entries: BTreeMap<String, BTreeMap<String, Value>>,
}

/// `data` field of the export result / start-import request (`{export, hash}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportPayload {
    pub export: ExportData,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportOutcome {
    /// `mockEntryId -> new entryId` for every imported entry.
    pub id_mapping: BTreeMap<String, String>,
    pub imported: usize,
    /// Scopes present in the export data that the current installation cannot import.
    pub skipped_scopes: Vec<String>,
}

/// Runtime configuration; every value comes from the same environment the monolith and the
/// Node meta-manager already use.
#[derive(Debug, Clone)]
pub struct TransferConfig {
    /// In-process US router (`/private/v2/...` routes need the master token).
    pub us_url: String,
    /// Public monolith base URL that hosts the internal ui-api transfer routes.
    pub internal_url: String,
    pub master_token: String,
    /// `EXPORT_DATA_VERIFICATION_KEY` — HMAC secret of `data.hash`.
    pub verification_key: String,
}

impl TransferConfig {
    pub fn from_env() -> Self {
        let internal_url = std::env::var("UI_PUBLIC_URL").unwrap_or_else(|_| {
            let port = std::env::var("PUBLIC_PORT").unwrap_or_else(|_| "8099".into());
            format!("http://127.0.0.1:{port}")
        });
        // The public monolith hosts the in-process US router; `US_URL` points at the
        // internal listener (39490), which has no /private US routes.
        let us_url = std::env::var("META_US_URL").unwrap_or_else(|_| internal_url.clone());
        Self {
            us_url,
            internal_url,
            master_token: std::env::var("US_MASTER_TOKEN").unwrap_or_default(),
            verification_key: std::env::var("EXPORT_DATA_VERIFICATION_KEY").unwrap_or_default(),
        }
    }
}

/// Copy the end-user identity headers the internal ui-api routes forward to
/// control-api/US. Same allowlist as `ui_api::auth_headers`; `reqwest` and `axum` share the
/// same `http` crate version, so a handler can pass its request headers directly.
pub fn forwarded_headers(source: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in [
        "authorization",
        "cookie",
        "x-dl-tenantid",
        "accept-language",
        "x-request-id",
    ] {
        let Some(value) = source.get(name).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        if let (Ok(value), Ok(name)) = (
            HeaderValue::from_str(value),
            HeaderName::from_bytes(name.as_bytes()),
        ) {
            out.insert(name, value);
        }
    }
    out
}

/// HTTP client for one transfer run. Cheap to clone: the `reqwest::Client` is shared and the
/// forwarded auth headers are small.
#[derive(Clone)]
pub struct MetaManager {
    http: reqwest::Client,
    config: TransferConfig,
    auth: HeaderMap,
}

impl MetaManager {
    pub fn new(config: TransferConfig, auth: HeaderMap) -> Self {
        Self {
            http: http_client(),
            config,
            auth,
        }
    }

    /// `auth` must be the initiating user's forwarded headers: the transfer outlives the
    /// HTTP request, so the handler has to copy them before returning.
    pub fn from_env(auth: HeaderMap) -> Self {
        Self::new(TransferConfig::from_env(), auth)
    }

    pub fn config(&self) -> &TransferConfig {
        &self.config
    }

    /// `data.hash` of an export payload: HMAC-SHA256 over the compact JSON of `data.export`.
    ///
    /// The Node implementation hashes `JSON.stringify(data.export)` whose object key order
    /// is insertion order; this implementation serializes `ExportData` (struct field order,
    /// sorted entry maps). A hash produced by one is therefore only self-consistent, not
    /// byte-compatible with the other — imports log a mismatch and continue, exactly like
    /// the Node meta-manager does on a hash mismatch.
    pub fn export_verification_hash(&self, data: &ExportData) -> String {
        verification_hash(&self.config.verification_key, data)
    }

    /// All entries of a workbook, following `nextPageToken` like the Node activity does.
    pub async fn list_entries(&self, workbook_id: &str) -> TransferResult<Vec<EntryRef>> {
        let mut entries = Vec::new();
        let mut page: u64 = 0;
        loop {
            let url = format!(
                "{}/private/v2/workbooks/{workbook_id}/entries?page={page}",
                self.config.us_url
            );
            let body = json_response(
                self.http
                    .get(url)
                    .headers(self.request_headers())
                    .send()
                    .await?,
            )
            .await?;
            if let Some(items) = body.get("entries").and_then(Value::as_array) {
                for item in items {
                    entries.push(serde_json::from_value(item.clone())?);
                }
            }
            match body.get("nextPageToken").and_then(Value::as_str) {
                Some(token) => {
                    page = token.parse().map_err(|_| {
                        TransferError::Failed(format!("invalid nextPageToken: {token}"))
                    })?;
                }
                None => break,
            }
        }
        Ok(entries)
    }

    /// Export one entry through the internal ui-api route.
    pub async fn export_entry(
        &self,
        workbook_id: &str,
        entry: &EntryRef,
        id_mapping: &BTreeMap<String, String>,
    ) -> TransferResult<ExportEntryResult> {
        let url = format!(
            "{}/api/internal/v1/workbooks/export",
            self.config.internal_url
        );
        let request = ExportEntryRequest {
            export_id: &entry.entry_id,
            scope: &entry.scope,
            id_mapping,
            workbook_id,
        };
        let body = json_response(
            self.http
                .post(url)
                .headers(self.request_headers())
                .json(&request)
                .send()
                .await?,
        )
        .await?;
        let result: ExportEntryResult = serde_json::from_value(body)?;
        // ui-api answers `{code: "ERR.UI_API.TRANSFER_INVALID_SCOPE"}` with HTTP 200.
        if result.entry_data.is_null() {
            return Err(TransferError::Failed(format!(
                "export of entry {} ({}) returned no entryData",
                entry.entry_id, entry.scope
            )));
        }
        Ok(result)
    }

    /// Import one entry into an existing workbook; `id_mapping` is the accumulated
    /// `mockEntryId -> new entryId` map.
    pub async fn import_entry(
        &self,
        workbook_id: &str,
        entry_data: &Value,
        id_mapping: &BTreeMap<String, String>,
    ) -> TransferResult<ImportEntryResult> {
        let url = format!(
            "{}/api/internal/v1/workbooks/import",
            self.config.internal_url
        );
        let request = ImportEntryRequest {
            id_mapping,
            workbook_id,
            entry_data,
        };
        let body = json_response(
            self.http
                .post(url)
                .headers(self.request_headers())
                .json(&request)
                .send()
                .await?,
        )
        .await?;
        let result: ImportEntryResult = serde_json::from_value(body)?;
        if result.id.is_none() {
            return Err(TransferError::Failed(
                "import route returned no entry id".into(),
            ));
        }
        Ok(result)
    }

    /// Export a whole workbook: list entries, walk them in scope-dependency order, then
    /// assemble the `{version, entries}` payload plus its verification hash.
    ///
    /// `notifications` is an out parameter so a failed run still reports the notifications
    /// collected up to the failure (the status route needs them).
    pub async fn export_workbook(
        &self,
        workbook_id: &str,
        cancel: &AtomicBool,
        notifications: &mut Vec<EntryNotification>,
        mut progress: impl FnMut(u8),
    ) -> TransferResult<ExportPayload> {
        let entries = self.list_entries(workbook_id).await?;
        let dependencies = default_dependencies();
        let order = resolve_order(&dependencies)?;
        let ordered = ordered_entries(&entries, &order);
        let mock_ids = mock_id_mapping(&entries);
        let total = ordered.len().max(1) as u32;

        let mut entry_data = BTreeMap::new();
        for (index, entry) in ordered.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(TransferError::Cancelled);
            }
            let result = self.export_entry(workbook_id, entry, &mock_ids).await?;
            let critical = result.notifications.iter().any(Notification::is_critical);
            notifications.extend(result.notifications.into_iter().map(|notification| {
                EntryNotification {
                    entry_id: Some(entry.entry_id.clone()),
                    scope: Some(entry.scope.clone()),
                    notification,
                }
            }));
            if critical {
                return Err(TransferError::Failed(format!(
                    "critical notification while exporting entry {}",
                    entry.entry_id
                )));
            }
            entry_data.insert(entry.entry_id.clone(), result.entry_data);
            progress(((index + 1) as u32 * 100 / total) as u8);
        }

        let export = assemble_export(&entries, &mock_ids, entry_data)?;
        Ok(ExportPayload {
            hash: self.export_verification_hash(&export),
            export,
        })
    }

    /// Import an export payload into an existing workbook, in scope-dependency order.
    /// Scopes the installation does not list in its capabilities are skipped with the same
    /// warning notification the Node workflow produces.
    pub async fn import_workbook(
        &self,
        workbook_id: &str,
        export: &ExportData,
        cancel: &AtomicBool,
        notifications: &mut Vec<EntryNotification>,
        mut progress: impl FnMut(u8),
    ) -> TransferResult<ImportOutcome> {
        let dependencies = default_dependencies();
        let order = resolve_order(&dependencies)?;
        let total: u32 = export
            .entries
            .values()
            .map(|scope| scope.len() as u32)
            .sum::<u32>()
            .max(1);
        let mut processed = 0u32;
        let mut id_mapping = BTreeMap::new();

        for batch in &order {
            for scope in batch {
                let Some(scope_entries) = export.entries.get(*scope) else {
                    continue;
                };
                for (mock_entry_id, entry_data) in scope_entries {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(TransferError::Cancelled);
                    }
                    let result = self
                        .import_entry(workbook_id, entry_data, &id_mapping)
                        .await?;
                    let critical = result.notifications.iter().any(Notification::is_critical);
                    let Some(entry_id) = result.id else {
                        return Err(TransferError::Failed(format!(
                            "import of entry {mock_entry_id} ({scope}) returned no id"
                        )));
                    };
                    notifications.extend(result.notifications.into_iter().map(|notification| {
                        EntryNotification {
                            // The Node import status reports the *new* entry id.
                            entry_id: Some(entry_id.clone()),
                            scope: Some((*scope).to_owned()),
                            notification,
                        }
                    }));
                    if critical {
                        return Err(TransferError::Failed(format!(
                            "critical notification while importing entry {mock_entry_id}"
                        )));
                    }
                    id_mapping.insert(mock_entry_id.clone(), entry_id);
                    processed += 1;
                    progress((processed * 100 / total) as u8);
                }
            }
        }

        let known: BTreeSet<&str> = order.iter().flatten().copied().collect();
        let skipped_scopes: Vec<String> = export
            .entries
            .keys()
            .filter(|scope| !known.contains(scope.as_str()))
            .cloned()
            .collect();
        for scope in &skipped_scopes {
            notifications.push(EntryNotification {
                entry_id: None,
                scope: Some(scope.clone()),
                notification: Notification {
                    code: NOTIF_SCOPE_NOT_AVAILABLE.into(),
                    level: Some("warning".into()),
                    message: Some(format!(
                        "Entries with scope \"{scope}\" is not available in current installation and will not be imported."
                    )),
                    details: None,
                },
            });
        }
        progress(100);
        Ok(ImportOutcome {
            id_mapping,
            imported: processed as usize,
            skipped_scopes,
        })
    }

    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.auth.clone();
        if let Ok(value) = HeaderValue::from_str(&self.config.master_token) {
            headers.insert("x-us-master-token", value);
        }
        headers
    }
}

/// Scope dependency graph reported by ui-api capabilities. Sorted map, so the resolved
/// order is `connection -> dataset -> widget -> dash` plus one batch per scope.
pub fn default_dependencies() -> BTreeMap<String, Vec<String>> {
    [
        ("connection", Vec::new()),
        ("dataset", vec!["connection".to_owned()]),
        (
            "widget",
            vec!["dataset".to_owned(), "connection".to_owned()],
        ),
        ("dash", vec!["widget".to_owned(), "dataset".to_owned()]),
    ]
    .into_iter()
    .map(|(scope, dependencies)| (scope.to_owned(), dependencies))
    .collect()
}

/// Batch scopes so every dependency is in an earlier (or the same, when it was already
/// resolved) batch. Same algorithm as the Node `resolveImportOrder`, including the
/// "circular dependency" error for a missing dependency name.
pub fn resolve_order(
    dependencies: &BTreeMap<String, Vec<String>>,
) -> TransferResult<Vec<Vec<&str>>> {
    let mut order: Vec<Vec<&str>> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while seen.len() < dependencies.len() {
        let batch: Vec<&str> = dependencies
            .iter()
            .filter(|(scope, scope_dependencies)| {
                !seen.contains(scope.as_str())
                    && scope_dependencies
                        .iter()
                        .all(|dependency| seen.contains(dependency.as_str()))
            })
            .map(|(scope, _)| scope.as_str())
            .collect();
        if batch.is_empty() {
            let stuck: Vec<&str> = dependencies
                .keys()
                .filter(|scope| !seen.contains(scope.as_str()))
                .map(String::as_str)
                .collect();
            return Err(TransferError::Failed(format!(
                "circular or missing dependency for scopes: {}",
                stuck.join(", ")
            )));
        }
        seen.extend(batch.iter().copied());
        order.push(batch);
    }
    Ok(order)
}

/// Node parity for `data.export`: the mock id of an entry is its index in the US entry
/// list as a decimal string. Exported entries are re-ordered for the transfer, but the
/// mock ids stay tied to the original list order.
pub fn mock_id_mapping(entries: &[EntryRef]) -> BTreeMap<String, String> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.entry_id.clone(), index.to_string()))
        .collect()
}

/// Entries in dependency order: one batch at a time, keeping the US order inside a batch.
/// Scopes the capabilities do not know go last in US order, so nothing silently disappears.
pub fn ordered_entries<'a>(entries: &'a [EntryRef], order: &[Vec<&str>]) -> Vec<&'a EntryRef> {
    let known: BTreeSet<&str> = order.iter().flatten().copied().collect();
    let mut ordered = Vec::with_capacity(entries.len());
    for batch in order {
        ordered.extend(
            entries
                .iter()
                .filter(|entry| batch.contains(&entry.scope.as_str())),
        );
    }
    ordered.extend(
        entries
            .iter()
            .filter(|entry| !known.contains(entry.scope.as_str())),
    );
    ordered
}

/// Assemble `data.export` from the US entry list, the mock id mapping and the exported
/// entry data. `entry_data` is consumed (the values are moved into the payload).
pub fn assemble_export(
    entries: &[EntryRef],
    mock_ids: &BTreeMap<String, String>,
    mut entry_data: BTreeMap<String, Value>,
) -> TransferResult<ExportData> {
    let mut export = ExportData {
        version: EXPORT_DATA_VERSION.to_owned(),
        entries: BTreeMap::new(),
    };
    for entry in entries {
        let Some(mock_id) = mock_ids.get(&entry.entry_id) else {
            return Err(TransferError::Failed(format!(
                "no mock id for entry {}",
                entry.entry_id
            )));
        };
        let Some(data) = entry_data.remove(&entry.entry_id) else {
            return Err(TransferError::Failed(format!(
                "no export data for entry {}",
                entry.entry_id
            )));
        };
        export
            .entries
            .entry(entry.scope.clone())
            .or_default()
            .insert(mock_id.clone(), data);
    }
    Ok(export)
}

/// HMAC-SHA256 of the compact JSON of `data.export`, hex encoded. Serialization of
/// `ExportData` (string keys, JSON values) cannot fail.
pub fn verification_hash(secret: &str, data: &ExportData) -> String {
    let json = serde_json::to_string(data).expect("ExportData is always JSON-serializable");
    hmac_sha256_hex(secret.as_bytes(), json.as_bytes())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEntryResult {
    #[serde(default)]
    pub notifications: Vec<Notification>,
    #[serde(default, rename = "entryData")]
    pub entry_data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEntryResult {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub notifications: Vec<Notification>,
}

#[derive(Serialize)]
struct ExportEntryRequest<'a> {
    #[serde(rename = "exportId")]
    export_id: &'a str,
    scope: &'a str,
    #[serde(rename = "idMapping")]
    id_mapping: &'a BTreeMap<String, String>,
    #[serde(rename = "workbookId")]
    workbook_id: &'a str,
}

#[derive(Serialize)]
struct ImportEntryRequest<'a> {
    #[serde(rename = "idMapping")]
    id_mapping: &'a BTreeMap<String, String>,
    #[serde(rename = "workbookId")]
    workbook_id: &'a str,
    #[serde(rename = "entryData")]
    entry_data: &'a Value,
}

fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // A hung internal call must not pin a job forever (Node activities used 20 s).
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

async fn json_response(response: reqwest::Response) -> TransferResult<Value> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(TransferError::Failed(format!("{status}: {body}")));
    }
    Ok(serde_json::from_str(&body)?)
}

// --- HMAC-SHA256 (no new dependency allowed: implemented locally, tested against RFC 4231) ---

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&sha256(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend(key_block.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(message);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend(key_block.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_hash);

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in sha256(&outer) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn sha256(data: &[u8]) -> [u8; 32] {
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut words = [0u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let first = words[index - 15];
            let second = words[index - 2];
            let s0 = first.rotate_right(7) ^ first.rotate_right(18) ^ (first >> 3);
            let s1 = second.rotate_right(17) ^ second.rotate_right(19) ^ (second >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(entry_id: &str, scope: &str) -> EntryRef {
        EntryRef {
            entry_id: entry_id.into(),
            scope: scope.into(),
        }
    }

    #[test]
    fn resolves_dependencies_in_batches() {
        let dependencies = default_dependencies();
        let order = resolve_order(&dependencies).unwrap();
        assert_eq!(
            order,
            vec![
                vec!["connection"],
                vec!["dataset"],
                vec!["widget"],
                vec!["dash"],
            ]
        );
    }

    #[test]
    fn rejects_circular_and_missing_dependencies() {
        let circular: BTreeMap<String, Vec<String>> = [
            ("a".to_owned(), vec!["b".to_owned()]),
            ("b".to_owned(), vec!["a".to_owned()]),
        ]
        .into_iter()
        .collect();
        assert!(matches!(
            resolve_order(&circular),
            Err(TransferError::Failed(message)) if message.contains("circular")
        ));

        let missing: BTreeMap<String, Vec<String>> = [("a".to_owned(), vec!["nope".to_owned()])]
            .into_iter()
            .collect();
        assert!(resolve_order(&missing).is_err());
    }

    #[test]
    fn mock_ids_follow_the_us_list_index() {
        let entries = vec![
            entry("conn", "connection"),
            entry("widget", "widget"),
            entry("dataset", "dataset"),
        ];
        assert_eq!(
            mock_id_mapping(&entries),
            [
                ("conn".to_owned(), "0".to_owned()),
                ("widget".to_owned(), "1".to_owned()),
                ("dataset".to_owned(), "2".to_owned()),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn orders_entries_by_scope_and_keeps_unknown_scopes_last() {
        let entries = vec![
            entry("w1", "widget"),
            entry("d1", "dash"),
            entry("c1", "connection"),
            entry("f1", "folder"),
            entry("ds1", "dataset"),
            entry("w2", "widget"),
        ];
        let dependencies = default_dependencies();
        let order = resolve_order(&dependencies).unwrap();
        let ordered: Vec<&str> = ordered_entries(&entries, &order)
            .into_iter()
            .map(|entry| entry.entry_id.as_str())
            .collect();
        assert_eq!(ordered, vec!["c1", "ds1", "w1", "w2", "d1", "f1"]);
    }

    #[test]
    fn assembles_export_grouped_by_scope_and_mock_id() {
        let entries = vec![entry("conn", "connection"), entry("w", "widget")];
        let mock_ids = mock_id_mapping(&entries);
        let entry_data: BTreeMap<String, Value> = [
            ("conn".to_owned(), json!({"connection": {"name": "pg"}})),
            ("w".to_owned(), json!({"widget": {"name": "chart"}})),
        ]
        .into_iter()
        .collect();

        let export = assemble_export(&entries, &mock_ids, entry_data).unwrap();
        assert_eq!(export.version, EXPORT_DATA_VERSION);
        assert_eq!(
            export.entries["connection"]["0"],
            json!({"connection": {"name": "pg"}})
        );
        assert_eq!(
            export.entries["widget"]["1"],
            json!({"widget": {"name": "chart"}})
        );
    }

    #[test]
    fn refuses_to_assemble_missing_entry_data() {
        let entries = vec![entry("w", "widget")];
        let mock_ids = mock_id_mapping(&entries);
        assert!(matches!(
            assemble_export(&entries, &mock_ids, BTreeMap::new()),
            Err(TransferError::Failed(message)) if message.contains("no export data")
        ));
    }

    #[test]
    fn sha256_known_vectors() {
        let hex = |digest: [u8; 32]| {
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        assert_eq!(
            hmac_sha256_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn verification_hash_matches_the_node_shape() {
        let data = ExportData {
            version: EXPORT_DATA_VERSION.into(),
            entries: BTreeMap::new(),
        };
        // Golden value independently computed with Python hmac/hashlib over
        // `{"version":"v1","entries":{}}` and the deploy secret.
        assert_eq!(
            verification_hash("export-data-verification-key", &data),
            "b05f579eed16d28981344eccb16a1fdeac369dfcdc65a7a5cfb40554a9207693"
        );
    }

    #[test]
    fn notification_criticality_follows_the_level() {
        let critical = Notification {
            code: "ERR.DS_API.WB_EXPORT.DS".into(),
            level: Some("critical".into()),
            ..Notification::default()
        };
        let warning = Notification {
            level: Some("warning".into()),
            ..Notification::default()
        };
        assert!(critical.is_critical());
        assert!(!warning.is_critical());
    }
}
