//! Direct US read model for the monolith chart path (v5.2).
//!
//! Before this module the engine's chart path resolved US metadata the long way:
//!
//! ```text
//! pipeline -> local_http::Client -> local_us mpsc channel -> US_ROUTER tower::oneshot
//! data-api -> dl_us::UsClient  -> local_us mpsc channel -> US_ROUTER tower::oneshot
//! ```
//!
//! Every dataset (`/fields`, `/result`) and every connection resolution therefore paid a
//! channel hop, a router call, JSON serialization/parsing and a second auth pass, even though
//! the US database pool already lives in this process.
//!
//! v5.2 exposes the typed read functions of the in-process `us-rs` server
//! (`us_rs::api::entries::read_entry_json` / `read_bundle`) and installs a
//! [`dl_us::DirectResolver`] so `dl_us::UsClient` (data-api) answers `/private/entries/{id}`
//! from the already-initialised US state instead of the channel. The metadata cache below is
//! **revision-validated**: every hit compares the cached entry's `saved_id`/`published_id`/
//! `updated_at` token with one cheap indexed replica query, so an edit or delete is visible on
//! the very next request; the TTL only bounds memory, never freshness.
//!
//! For one chart request the engine fetches a **request-scoped bundle** in a single call —
//! the chart entry, its datasets and the datasets' connections — and `with_bundle` makes it
//! visible to every inline US read (engine and data-api) for the duration of the dispatch.
//! A bundle miss falls back to the same direct SQL read, never to the channel.
//!
//! Wiring: `main.rs` (`mod us_direct;`), `us_main.rs` (`install(state)` after the US state is
//! built) and `pipeline.rs` (`entry_json` + `bundle`/`with_bundle` around the chart dispatch).
//! The exact diff is in `patches/us_direct.md`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::{json, Value};
use us_rs::api::entries::EntryFreshness;

/// The initialised `us-rs` state (same `Db` pools + config the US server serves from).
static US_STATE: OnceLock<us_rs::state::AppState> = OnceLock::new();
/// Guards the one-time `dl_us` resolver installation.
static RESOLVER: OnceLock<()> = OnceLock::new();

// ---------------------------------------------------------------------------------------------
// Request access context (who is asking, for which workbook, sudo flags)
// ---------------------------------------------------------------------------------------------
//
// The engine reads US metadata in-process, so it must reproduce the checks the US HTTP route
// performs for the caller: workbook isolation (`x-dl-workbookid`) and per-user cache separation.
// `scope_identity` installs this context around the whole `/api/run`; trusted internal readers
// (no context) keep the service identity.

#[derive(Clone, Debug, Default)]
pub struct AccessContext {
    /// `user|groups|roles`, the same shape the entry-cache key uses; `service` when trustless.
    pub identity: String,
    /// `x-dl-workbookid` of the request: an entry from another workbook must not be served.
    pub workbook_id: Option<i64>,
    /// `x-dl-sudo` / `x-dl-allow-superuser` participation in the cache key (semantics are
    /// applied by US; keeping them in the key stops a sudo read from being replayed without it).
    pub sudo: bool,
    pub allow_superuser: bool,
}

tokio::task_local! {
    static ACCESS: AccessContext;
}

/// Run `future` with the caller's access context visible to every US read on this task.
pub async fn with_access<F: Future>(access: AccessContext, future: F) -> F::Output {
    ACCESS.scope(access, future).await
}

fn current_access() -> Option<AccessContext> {
    ACCESS.try_with(Clone::clone).ok()
}

/// Entry id from an encoded US id, or `None` when the header is absent/invalid.
pub fn decode_entry_id(raw: &str) -> Option<i64> {
    us_rs::id::decode_id(raw).ok()
}

// ---------------------------------------------------------------------------------------------
// Process-wide US entry cache
// ---------------------------------------------------------------------------------------------
//
// v9.1 read the chart entry (32-104 KB JSON), its dataset (46-82 KB) and the connection from
// `us` **on every `/api/run`**: SQL + `jsonb` decode + `serde_json::to_value` rebuild + typed
// dataset decode. The request-scoped bundle only deduplicated those reads *inside* one
// request, so a 21-chart dashboard still paid ~60 US reads.
//
// The cache below keeps the parsed entry (`Arc<Value>`) and the decoded typed dataset for
// `US_ENTRY_CACHE_TTL_SECS` (default 30 s). The key carries tenant + caller identity + branch,
// so a per-user read is never served to another user, and the `revId` is part of the entry —
// a revision switch is picked up by the next read after the TTL (the same freshness policy the
// `/fields` cache already uses, `DATASET_FIELDS_TTL_SECS=60`).

/// One cached `GET /private/entries/{id}` answer.
struct CachedEntry {
    stored: Instant,
    value: Arc<Value>,
    revision: Option<String>,
    /// Proof that the cached bytes are still the current revision: the entry's
    /// `saved_id`/`published_id`/`updated_at` at the moment it was read.
    token: EntryFreshness,
}

/// Cache identity: tenant + caller + branch + entry id.
#[derive(Clone, PartialEq, Eq, Hash)]
struct EntryKey {
    tenant: String,
    identity: String,
    branch: String,
    entry: String,
}

static ENTRY_CACHE: OnceLock<Mutex<HashMap<EntryKey, CachedEntry>>> = OnceLock::new();
/// In-flight coalescing: concurrent requests that miss the same entry/dataset/connection key wait
/// for the first US read instead of issuing their own SQL + jsonb decode. Dashboard charts start
/// together and share datasets/connections, so on a cold cache this turns N reads into one.
static ENTRY_INFLIGHT: OnceLock<Mutex<HashMap<EntryKey, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();
static ENTRY_HITS: AtomicU64 = AtomicU64::new(0);
static ENTRY_MISSES: AtomicU64 = AtomicU64::new(0);
static ENTRY_STALE: AtomicU64 = AtomicU64::new(0);
static ENTRY_STORES: AtomicU64 = AtomicU64::new(0);

/// Offline US mock: `US_MOCK_DIR` points at one JSON file per entry id. Used by the engine-only
/// stress harness so no US SQL runs at all (files are read once and kept as `Arc<Value>`).
static US_MOCK: OnceLock<Mutex<HashMap<String, Arc<Value>>>> = OnceLock::new();

fn us_mock_dir() -> Option<String> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var("US_MOCK_DIR")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
    .clone()
}

fn us_mock_entry(entry_id: &str) -> Option<Arc<Value>> {
    let dir = us_mock_dir()?;
    let cache = US_MOCK.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(value) = cache.get(entry_id) {
            return Some(Arc::clone(value));
        }
    }
    if entry_id.contains('/') || entry_id.contains('\\') || entry_id.contains("..") {
        return None;
    }
    // Sharded metadata mock: emulate entries partitioned across `US_MOCK_SHARDS`
    // metadata shards. No network/DNS/balancer hop in the hot path; the shard choice is
    // the same 131-hash the fixture generator uses.
    let mut candidates = vec![std::path::Path::new(&dir).join(format!("{entry_id}.json"))];
    if let Ok(raw) = std::env::var("US_MOCK_SHARDS") {
        if let Ok(count) = raw.trim().parse::<usize>() {
            if count > 0 {
                let mut hash: u32 = 0;
                for ch in entry_id.bytes() {
                    hash = hash.wrapping_mul(131).wrapping_add(ch as u32);
                }
                candidates.push(
                    std::path::Path::new(&dir)
                        .join((hash as usize % count).to_string())
                        .join(format!("{entry_id}.json")),
                );
            }
        }
    }
    let text = candidates
        .into_iter()
        .find_map(|candidate| std::fs::read_to_string(candidate).ok())?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let value = Arc::new(value);
    if let Ok(mut cache) = cache.lock() {
        cache.insert(entry_id.to_string(), Arc::clone(&value));
    }
    Some(value)
}

fn entry_cache() -> &'static Mutex<HashMap<EntryKey, CachedEntry>> {
    ENTRY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `US_ENTRY_CACHE_TTL_SECS` (**default 60 s**).
///
/// The TTL only decides how long a parsed entry may stay in memory; **freshness is enforced
/// by the revision token**, not by the TTL: every hit re-reads `saved_id`, `published_id` and
/// `updated_at` from the replica and drops the entry when any of them moved. An edit or a
/// delete is therefore visible on the next request even with a long TTL. `0` turns the cache
/// off entirely (diagnostics / cold comparison). The request-scoped bundle still deduplicates
/// the reads inside one `/api/run`.
fn entry_cache_ttl() -> Duration {
    static TTL: OnceLock<Duration> = OnceLock::new();
    *TTL.get_or_init(|| {
        let seconds = std::env::var("US_ENTRY_CACHE_TTL_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(60);
        Duration::from_secs(seconds)
    })
}

fn entry_cache_max() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("US_ENTRY_CACHE_MAX")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(1024)
    })
}

fn cache_tenant() -> String {
    static TENANT: OnceLock<String> = OnceLock::new();
    TENANT
        .get_or_init(|| std::env::var("TENANT_ID_OVERRIDE").unwrap_or_else(|_| "common".to_string()))
        .clone()
}

fn cache_identity() -> String {
    if let Some(access) = current_access() {
        let mut identity = access.identity;
        if access.sudo {
            identity.push_str("|sudo");
        }
        if access.allow_superuser {
            identity.push_str("|allow-superuser");
        }
        return identity;
    }
    // Trusted internal reader (no `/api/run` context): the service identity.
    match dl_query::identity::current() {
        None => "service".to_string(),
        Some(identity) => format!(
            "{}|{}|{}",
            identity.user_id,
            identity.groups.join(","),
            identity.roles.join(",")
        ),
    }
}

fn entry_key(entry_id: &str, branch: &str) -> EntryKey {
    EntryKey {
        tenant: cache_tenant(),
        identity: cache_identity(),
        branch: if branch == "saved" { "saved" } else { "published" }.to_string(),
        entry: entry_id.to_string(),
    }
}

/// The US route's workbook-isolation check for an in-process read.
///
/// `get_entry` rejects a request whose `x-dl-workbookid` names another workbook before any
/// permission/row data is returned. The direct read bypasses the HTTP handler, so this runs on
/// every cache hit and before every cache store: isolation is never a property of the cache.
fn validate_access(value: &Value) -> Result<(), us_rs::error::ApiError> {
    let Some(access) = current_access() else {
        return Ok(());
    };
    let Some(expected) = access.workbook_id else {
        return Ok(());
    };
    let actual = value
        .get("workbookId")
        .and_then(Value::as_str)
        .and_then(decode_entry_id);
    if actual == Some(expected) {
        return Ok(());
    }
    Err(us_rs::error::ApiError::new(
        axum::http::StatusCode::FORBIDDEN,
        Some("WORKBOOK_ISOLATION_INTERRUPTION"),
        "The entry does not belong to the requested workbook",
    ))
}

/// Cached value for `key`, or `None` when absent, expired, or **stale**.
///
/// A hit is served only after the entry's revision token still matches the cache; that makes
/// an edit/delete visible immediately (no TTL window), while `Arc` keeps the 32-104 KB parse
/// shared between the dashboard's charts.
async fn cached_value(
    state: &us_rs::state::AppState,
    key: &EntryKey,
    entry_id: &str,
) -> Option<Arc<Value>> {
    let ttl = entry_cache_ttl();
    if ttl.is_zero() {
        return None;
    }
    let (value, token, expired) = {
        let cache = entry_cache().lock().ok()?;
        let entry = cache.get(key)?;
        (Arc::clone(&entry.value), entry.token, entry.stored.elapsed() >= ttl)
    };
    if expired {
        ENTRY_MISSES.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    match us_rs::api::entries::entry_freshness(state, entry_id).await {
        Ok(Some(current)) if current == token => {
            if validate_access(&value).is_err() {
                ENTRY_STALE.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            ENTRY_HITS.fetch_add(1, Ordering::Relaxed);
            Some(value)
        }
        Ok(_) => {
            // Revision switched, entry deleted or the row is gone: never serve the old bytes.
            ENTRY_STALE.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut cache) = entry_cache().lock() {
                cache.remove(key);
            }
            None
        }
        Err(_) => {
            // US is unreachable: do not guess freshness; fall through to the direct read so
            // the request gets the same error it would have got without the cache.
            ENTRY_MISSES.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Store one entry, dropping expired ones first so the map stays bounded.
fn store_value(key: EntryKey, value: Arc<Value>, token: EntryFreshness) {
    let ttl = entry_cache_ttl();
    if ttl.is_zero() {
        return;
    }
    let Ok(mut cache) = entry_cache().lock() else {
        return;
    };
    if cache.len() >= entry_cache_max() && !cache.contains_key(&key) {
        cache.retain(|_, entry| entry.stored.elapsed() < ttl);
        if cache.len() >= entry_cache_max() {
            cache.clear();
        }
    }
    let revision = value.get("revId").and_then(Value::as_str).map(str::to_string);
    ENTRY_STORES.fetch_add(1, Ordering::Relaxed);
    cache.insert(
        key,
        CachedEntry {
            stored: Instant::now(),
            value,
            revision,
            token,
        },
    );
}

fn entry_inflight() -> &'static Mutex<HashMap<EntryKey, Arc<tokio::sync::Mutex<()>>>> {
    ENTRY_INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn inflight_lock(key: &EntryKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = match entry_inflight().lock() {
        Ok(map) => map,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(
        map.entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// Removes the in-flight entry when the reader ends, **including cancellation**.
///
/// A request dropped at an await (client disconnect, the `/api/run` deadline) used to skip
/// `inflight_done`, so its key stayed in the map for the life of the process.
struct Inflight {
    key: EntryKey,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Inflight {
    fn new(key: EntryKey) -> Self {
        let lock = inflight_lock(&key);
        Self { key, lock }
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        // Map + this guard are the last holders: a waiter still holding a clone removes the
        // entry itself when it finishes, exactly as before.
        if Arc::strong_count(&self.lock) == 2 {
            if let Ok(mut map) = entry_inflight().lock() {
                map.remove(&self.key);
            }
        }
    }
}

/// Read one entry through the revision-validated cache, falling back to the US replica pool.
///
/// The miss path reads the token, the entry and the token again: storing only when both tokens
/// agree closes the race "edit lands between the entry read and the token read", which would
/// otherwise cache the old bytes under the new token.
async fn read_entry_cached(
    state: &us_rs::state::AppState,
    entry_id: &str,
    branch: &str,
) -> Result<Arc<Value>, us_rs::error::ApiError> {
    if let Some(value) = us_mock_entry(entry_id) {
        return Ok(value);
    }
    let key = entry_key(entry_id, branch);
    if let Some(value) = cached_value(state, &key, entry_id).await {
        return Ok(value);
    }
    // Cold miss: only the first task reads US; the rest wake up and take the fresh cache entry
    // (a dashboard's charts resolve the same datasets/connections concurrently on a cold start).
    let inflight = Inflight::new(key.clone());
    let _permit = inflight.lock.lock().await;
    let result: Result<Arc<Value>, us_rs::error::ApiError> = async {
        if let Some(value) = cached_value(state, &key, entry_id).await {
            return Ok(value);
        }
        let before = us_rs::api::entries::entry_freshness(state, entry_id).await?;
        let value =
            Arc::new(us_rs::api::entries::read_entry_json(state, entry_id, branch, None).await?);
        validate_access(&value)?;
        if let Some(before) = before {
            if let Ok(Some(after)) = us_rs::api::entries::entry_freshness(state, entry_id).await {
                if after == before {
                    store_value(key.clone(), Arc::clone(&value), after);
                }
            }
        }
        Ok(value)
    }
    .await;
    result
}

/// Decode one entry `Value` into the typed dataset data-api works with.
fn decode_typed_dataset(
    entry: &Value,
    entry_id: &str,
) -> Option<(Arc<dl_core::Dataset>, Option<String>)> {
    if entry.get("scope").and_then(Value::as_str) != Some("dataset") {
        return None;
    }
    let revision = entry
        .get("revId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let data = entry.get("data")?.clone();
    let data: dl_us::UsDatasetData = serde_json::from_value(data).ok()?;
    let dataset = Arc::new(dl_us::to_dataset(entry_id, &data).ok()?);
    Some((dataset, revision))
}

/// `/debug/cache` view of this cache: sizes and counters, including `stale` (a hit rejected by
/// the revision token) — the observable proof that freshness is enforced, not TTL-guessed.
/// Drop every cached US entry (idle memory trimmer). The revision token re-validates on the
/// next read, so a stale-free cache is rebuilt with the same freshness guarantees.
pub fn clear_entry_cache() -> usize {
    let Some(cache) = ENTRY_CACHE.get() else {
        return 0;
    };
    let Ok(mut cache) = cache.lock() else {
        return 0;
    };
    let dropped = cache.len();
    cache.clear();
    dropped
}

pub fn entry_cache_json() -> Value {
    let size = entry_cache().lock().map(|cache| cache.len()).unwrap_or(0);
    json!({
        "size": size,
        "max": entry_cache_max(),
        "ttl_secs": entry_cache_ttl().as_secs(),
        "hits": ENTRY_HITS.load(Ordering::Relaxed),
        "misses": ENTRY_MISSES.load(Ordering::Relaxed),
        "stale": ENTRY_STALE.load(Ordering::Relaxed),
        "stores": ENTRY_STORES.load(Ordering::Relaxed),
        "validated": true,
    })
}

/// `data.shared.datasetsIds` of an entry (the `shared` block is often a JSON string).
fn dataset_ids_from_entry(entry: &Value) -> Vec<String> {
    let shared = entry.pointer("/data/shared");
    let shared = match shared {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let Some(shared) = shared else {
        return Vec::new();
    };
    shared
        .get("datasetsIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Environment switch for A/B diagnostics (`US_DIRECT`, `US_DIRECT_BUNDLE`).
fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|value| !matches!(value.trim(), "0" | "false" | "off" | "no"))
        .unwrap_or(default)
}

tokio::task_local! {
    /// Request-scoped entry/dataset/connection bundle (set by `with_bundle`).
    static BUNDLE: Arc<Bundle>;
}

/// Register the `us-rs` state. Called by `us_main` right after the Db pool is ready and
/// before the US router starts answering, so the engine never sees a half-installed model.
pub fn install(state: us_rs::state::AppState) {
    let _ = US_STATE.set(state);
    if env_flag("US_DIRECT", true) {
        install_dl_us_resolver();
        tracing::info!("direct US read model installed");
    } else {
        tracing::warn!("direct US read model disabled by US_DIRECT=0; using the local_us channel");
    }
}

/// Install the `dl_us` direct resolver: data-api's `UsClient` (`inproc://us`) then reads
/// entries from the request-scoped bundle or the US replica pool, with no channel and no
/// `tower::oneshot` router round trip.
pub fn install_dl_us_resolver() {
    if RESOLVER.set(()).is_err() {
        return;
    }
    // 1. Typed dataset source: `data-api` gets `Arc<Dataset>` for the request's datasets
    //    straight from this bundle — no entry JSON, no `to_dataset` per `/fields` or `/result`.
    dl_us::install_dataset_direct(Arc::new(bundled_dataset));
    // 2. Value source: entry reads return the already-parsed JSON Value, so the client does
    //    not serialize the entry to bytes and parse those bytes straight back.
    let value_resolver: dl_us::ValueResolver = Arc::new(|path, headers| {
        Box::pin(async move { resolve_local_value(path, headers).await })
    });
    dl_us::install_value_direct(value_resolver);
    // 3. Byte transport: the legacy channel-compatible resolver, still used for anything the
    //    value/dataset resolvers do not answer.
    let resolver: dl_us::DirectResolver = Arc::new(|path, headers| {
        Box::pin(async move { resolve_local(path, headers).await })
    });
    dl_us::install_direct(resolver);
}

/// The typed dataset of the current request-scoped bundle, else the process-wide entry cache.
fn bundled_dataset(entry_id: &str) -> Option<(Arc<dl_core::Dataset>, Option<String>)> {
    let from_bundle = BUNDLE
        .try_with(|bundle| bundle.typed_dataset(entry_id))
        .ok()
        .flatten();
    // Deliberately bundle-only: the process-wide cache is revision-validated for `Value`s
    // (async), while this resolver is synchronous. A non-bundle request re-reads the entry
    // through the validated value path instead of risking a stale typed dataset.
    from_bundle
}

/// `DL_US_BRANCH` (the branch every engine US read uses), cached for process lifetime.
fn current_branch() -> String {
    static BRANCH: OnceLock<String> = OnceLock::new();
    BRANCH
        .get_or_init(|| std::env::var("DL_US_BRANCH").unwrap_or_else(|_| "published".to_string()))
        .clone()
}

/// One request-scoped US bundle: chart entry + datasets + connections, indexed by entry id.
///
/// Entries are kept as `Arc<Value>` so repeated lookups in the request (engine chart entry,
/// data-api dataset, data-api connection) do not clone the (sometimes large) dataset schema.
#[derive(Default)]
pub struct Bundle {
    by_id: HashMap<String, Arc<Value>>,
    datasets: usize,
    connections: usize,
    /// Typed `dl_core::Dataset` per dataset entry, decoded from the bundle **once** per
    /// request. Before this, every data-api call (`/fields`, each `/result`) re-read the
    /// dataset JSON, re-parsed it into `UsEntry`, re-decoded `UsDatasetData` and re-built the
    /// typed dataset. Every chart does 2-3 of those calls, so one request parsed the same
    /// metadata 2-3 times.
    typed: Mutex<HashMap<String, Arc<dl_core::Dataset>>>,
}

impl Bundle {
    fn from_us(bundle: us_rs::api::entries::UsReadBundle) -> Self {
        let mut this = Self::default();
        this.datasets = bundle.datasets.len();
        this.connections = bundle.connections.len();
        if let Some(entry) = bundle.entry {
            this.insert(entry);
        }
        for dataset in bundle.datasets {
            this.insert(dataset);
        }
        for connection in bundle.connections {
            this.insert(connection);
        }
        this
    }

    fn insert(&mut self, entry: Value) {
        self.insert_arc(Arc::new(entry));
    }

    fn insert_arc(&mut self, entry: Arc<Value>) {
        let Some(entry_id) = entry
            .get("entryId")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        self.by_id.insert(entry_id, entry);
    }

    /// The `revId` of an entry in this bundle (a dataset entry carries the dataset revision
    /// the chart request is pinned to), if the bundle has it.
    fn revision(&self, raw_entry_id: &str) -> Option<String> {
        self.lookup(raw_entry_id)
            .and_then(|entry| entry.get("revId").and_then(Value::as_str).map(str::to_string))
    }

    fn lookup(&self, raw_entry_id: &str) -> Option<Arc<Value>> {
        if let Some(entry) = self.by_id.get(raw_entry_id) {
            return Some(Arc::clone(entry));
        }
        // `dl_us` percent-encodes the id into the path; ids are normally UUID-like, but a
        // plain `%`-decoded form is tried as well.
        let decoded = percent_decode(raw_entry_id);
        self.by_id.get(&decoded).cloned()
    }

    /// The typed dataset for `entry_id`, built from the bundled entry the first time it is
    /// asked for and shared by every later data-api call of this request.
    fn typed_dataset(&self, entry_id: &str) -> Option<(Arc<dl_core::Dataset>, Option<String>)> {
        let entry = self.lookup(entry_id)?;
        let revision = entry
            .get("revId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut typed = self.typed.lock().ok()?;
        if let Some(dataset) = typed.get(entry_id) {
            return Some((Arc::clone(dataset), revision));
        }
        let (dataset, _) = decode_typed_dataset(&entry, entry_id)?;
        typed.insert(entry_id.to_string(), Arc::clone(&dataset));
        Some((dataset, revision))
    }
}

/// The pinned revision of `dataset_id` from the request-scoped US bundle, if the bundle has
/// it. Used by the metric path to send `dataset_revision_id`, so the data-api pins the
/// dataset and the query-result cache key carries the revision (task-25 / verifier T5).
pub fn bundled_revision(dataset_id: &str) -> Option<String> {
    BUNDLE
        .try_with(|bundle| bundle.revision(dataset_id))
        .ok()
        .flatten()
}

/// `shared.datasetsIds` of a chart request (normalized to `[String]`).
pub fn dataset_ids(shared: &Value) -> Vec<String> {
    shared
        .get("datasetsIds")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `US_DIRECT_BUNDLE` is deployment-static; cache it instead of reading the environment
/// on every chart request.
fn bundle_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("US_DIRECT_BUNDLE", true))
}

/// Fetch the request-scoped US bundle in one call: `chart_entry_id` (when the request names
/// one) + `dataset_ids` + the datasets' connections. Every read goes through the process-wide
/// entry cache (`US_ENTRY_CACHE_TTL_SECS`), so a dashboard's charts share one `Arc<Value>` per
/// object instead of re-reading the same 32-104 KB entry 21 times. A failure is logged and
/// yields an empty bundle: the dispatch still works through the direct resolver.
pub async fn bundle(chart_entry_id: Option<&str>, dataset_ids: &[String], branch: &str) -> Bundle {
    if !bundle_enabled() {
        return Bundle::default();
    }
    let Some(state) = US_STATE.get() else {
        return Bundle::default();
    };
    let mut bundle = Bundle::default();
    let entry = match chart_entry_id {
        Some(entry_id) => match read_entry_cached(state, entry_id, branch).await {
            Ok(entry) => Some(entry),
            Err(error) => {
                tracing::warn!(
                    code = error.code.unwrap_or_default(),
                    error = %error.message,
                    entry_id,
                    "US bundle: chart entry read failed; falling back to per-entry reads"
                );
                return Bundle::default();
            }
        },
        None => None,
    };

    let mut ids: Vec<String> = Vec::new();
    for id in dataset_ids {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.clone());
        }
    }
    if ids.is_empty() {
        if let Some(entry) = &entry {
            ids = dataset_ids_from_entry(entry);
        }
    }

    let mut connection_ids: Vec<String> = Vec::new();
    for id in &ids {
        let dataset = match read_entry_cached(state, id, branch).await {
            Ok(dataset) => dataset,
            Err(_) => return Bundle::default(),
        };
        if let Some(connection_id) = us_rs::api::entries::dataset_connection_id(&dataset) {
            if !connection_ids.contains(&connection_id) {
                connection_ids.push(connection_id);
            }
        }
        bundle.datasets += 1;
        bundle.insert_arc(dataset);
    }
    for id in &connection_ids {
        let connection = match read_entry_cached(state, id, branch).await {
            Ok(connection) => connection,
            Err(_) => return Bundle::default(),
        };
        bundle.connections += 1;
        bundle.insert_arc(connection);
    }
    if let Some(entry) = entry {
        bundle.insert_arc(entry);
    }
    tracing::debug!(
        entry = chart_entry_id.unwrap_or_default(),
        entries = bundle.by_id.len(),
        datasets = bundle.datasets,
        connections = bundle.connections,
        "US direct bundle (cached)"
    );
    bundle
}

/// Run `future` with the bundle visible to every US read on this task (the engine and the
/// in-process data-api handler share the task, so `dl_us` resolves from the bundle).
pub async fn with_bundle<F: Future>(bundle: Bundle, future: F) -> F::Output {
    BUNDLE.scope(Arc::new(bundle), future).await
}

/// The bundle entry for `raw_entry_id`, if one was pre-read for this request.
fn bundled_entry(raw_entry_id: &str) -> Option<Arc<Value>> {
    BUNDLE
        .try_with(|bundle| bundle.lookup(raw_entry_id))
        .ok()
        .flatten()
}

/// Direct `GET /private/entries/{id}` equivalent used by the chart dispatch instead of
/// `local_http::Client -> local_us -> US_ROUTER`. Returns the same JSON body shape.
pub async fn entry_json(
    raw_entry_id: &str,
    branch: &str,
    rev_id: Option<&str>,
) -> Result<Arc<Value>, (StatusCode, String)> {
    if let Some(entry) = bundled_entry(raw_entry_id) {
        tracing::debug!(entry_id = raw_entry_id, "US direct entry: bundle hit");
        // `Arc`, not `(*entry).clone()`: the chart config is 32-104 KB and the dispatch only
        // borrows it.
        return Ok(entry);
    }
    let Some(state) = US_STATE.get() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "US direct read model is not installed".to_string(),
        ));
    };
    // With an explicit `rev_id` the caller pins a revision: read it directly (the cache key
    // does not carry a revision, and a pinned read must never be answered from a newer entry).
    // A US 401/403/404 is the caller's answer, not a gateway failure: the direct read must
    // not turn an ACL/isolation rejection into a 502.
    let status_for = |error: &us_rs::error::ApiError| {
        if matches!(error.status.as_u16(), 400 | 401 | 403 | 404) {
            error.status
        } else {
            StatusCode::BAD_GATEWAY
        }
    };
    if rev_id.is_some() {
        return us_rs::api::entries::read_entry_json(state, raw_entry_id, branch, rev_id)
            .await
            .map(Arc::new)
            .map_err(|error| {
                (
                    status_for(&error),
                    format!("us: {}{}", error.message, error.code.unwrap_or_default()),
                )
            });
    }
    read_entry_cached(state, raw_entry_id, branch).await.map_err(|error| {
        (
            status_for(&error),
            format!("us: {}{}", error.message, error.code.unwrap_or_default()),
        )
    })
}

/// Value variant of [`resolve_local`]: the same answer, already parsed. The client decodes
/// this `Value` straight into `UsEntry`, so the entry is never serialized to JSON text and
/// parsed back on the in-process hop.
async fn resolve_local_value(
    path: String,
    _headers: Vec<(String, String)>,
) -> Option<(u16, Arc<Value>)> {
    let (entry_id, branch) = parse_entry_path(&path)?;
    if let Some(entry) = bundled_entry(&entry_id) {
        tracing::debug!(entry_id, "US direct resolver: bundle hit (value)");
        return Some((200, entry));
    }
    let Some(state) = US_STATE.get() else {
        // Dry-run / unit contexts: let the byte resolver (or the channel) answer.
        return None;
    };
    tracing::debug!(entry_id, bundle = false, "US direct resolver: direct sql (value)");
    match us_rs::api::entries::read_entry_json(state, &entry_id, &branch, None).await {
        Ok(entry) => Some((200, Arc::new(entry))),
        Err(error) => Some((
            error.status.as_u16(),
            Arc::new(serde_json::json!({
                "code": error.code,
                "message": error.message,
            })),
        )),
    }
}

/// `dl_us` direct resolver: `LocalRequest`-shaped path + headers -> JSON response.
async fn resolve_local(path: String, headers: Vec<(String, String)>) -> Option<dl_us::LocalResponse> {
    let (entry_id, branch) = parse_entry_path(&path)?;

    if let Some(entry) = bundled_entry(&entry_id) {
        tracing::debug!(entry_id, "US direct resolver: bundle hit");
        return Some(json_response(200, &entry));
    }

    let Some(state) = US_STATE.get() else {
        // Dry-run / unit contexts: keep the legacy in-process channel transport.
        return crate::local_us::call(path, headers).await;
    };

    tracing::debug!(entry_id, bundle = false, "US direct resolver: direct sql");
    match us_rs::api::entries::read_entry_json(state, &entry_id, &branch, None).await {
        Ok(entry) => Some(json_response(200, &entry)),
        Err(error) => Some(error_response(&error)),
    }
}

/// `/private/entries/{id}?branch={branch}` -> (decoded id, branch). `None` means "not an
/// entry read" and falls back to the legacy transport.
fn parse_entry_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/private/entries/")?;
    let (raw_id, query) = rest.split_once('?').unwrap_or((rest, ""));
    if raw_id.is_empty() {
        return None;
    }
    let mut branch = "published".to_string();
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name == "branch" && !value.is_empty() {
                branch = percent_decode(value);
            }
        }
    }
    Some((percent_decode(raw_id), branch))
}

fn json_response(status: u16, value: &Value) -> dl_us::LocalResponse {
    dl_us::LocalResponse {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(value).unwrap_or_default(),
    }
}

fn error_response(error: &us_rs::error::ApiError) -> dl_us::LocalResponse {
    let body = serde_json::json!({
        "code": error.code,
        "message": error.message,
    });
    dl_us::LocalResponse {
        status: error.status.as_u16(),
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&body).unwrap_or_default(),
    }
}

/// Minimal `%xx` decoder (ids are UUID-like; this only makes the fallback forms work).
fn percent_decode(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| -> Option<u8> {
                match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_with_dataset() -> Bundle {
        Bundle::from_us(us_rs::api::entries::UsReadBundle {
            entry: None,
            datasets: vec![dl_us::testing::dataset_entry()],
            connections: Vec::new(),
        })
    }

    /// The request-scoped bundle decodes each dataset **once** into a typed
    /// `Arc<dl_core::Dataset>`; `/fields` and every `/result` of the chart then share it
    /// instead of re-reading and re-decoding the entry JSON (the old behaviour).
    #[test]
    fn a_bundled_dataset_is_decoded_once_and_shared() {
        let bundle = bundle_with_dataset();
        let (first, revision) = bundle.typed_dataset("dataset-1").expect("typed dataset");
        assert_eq!(revision.as_deref(), Some("rev-1"));
        assert!(!first.fields.is_empty());
        let (second, _) = bundle.typed_dataset("dataset-1").expect("second lookup");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same decoded dataset must be handed to every data-api call"
        );
    }

    #[test]
    fn shared_dataset_ids_are_read_from_a_string_or_an_object() {
        let as_string = serde_json::json!({
            "entryId": "chart-1",
            "data": {"shared": "{\"datasetsIds\": [\"ds-1\", \"ds-2\"]}"}
        });
        assert_eq!(dataset_ids_from_entry(&as_string), vec!["ds-1", "ds-2"]);
        let as_object = serde_json::json!({
            "entryId": "chart-1",
            "data": {"shared": {"datasetsIds": ["ds-3"]}}
        });
        assert_eq!(dataset_ids_from_entry(&as_object), vec!["ds-3"]);
        assert!(dataset_ids_from_entry(&serde_json::json!({"data": {}})).is_empty());
    }

    #[test]
    fn a_non_dataset_entry_is_not_typed_here() {
        let bundle = Bundle::from_us(us_rs::api::entries::UsReadBundle {
            entry: Some(serde_json::json!({
                "entryId": "chart-1",
                "revId": "rev-2",
                "scope": "chart",
                "data": {"shared": "{}"}
            })),
            datasets: Vec::new(),
            connections: Vec::new(),
        });
        assert!(bundle.typed_dataset("chart-1").is_none());
        assert!(bundle.typed_dataset("missing").is_none());
    }

    /// A reader dropped at an await (deadline, client disconnect) must not leave its key in
    /// the in-flight map: the next cold read of that entry would queue behind a stale mutex.
    #[tokio::test]
    async fn a_dropped_reader_clears_its_inflight_entry() {
        let key = EntryKey {
            tenant: "t".into(),
            identity: "i".into(),
            branch: "published".into(),
            entry: "guard-drop".into(),
        };
        let inflight = Inflight::new(key.clone());
        assert!(
            entry_inflight().lock().expect("inflight").contains_key(&key),
            "the leader must own the entry while it reads"
        );
        drop(inflight);
        assert!(
            !entry_inflight().lock().expect("inflight").contains_key(&key),
            "the entry must be removed when the reader ends"
        );
    }
}
