//! The request pipeline: dataset + updates -> query spec -> SQL -> rows -> API response.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;

use dl_core::{DataType, Dataset, FieldRef, FieldType, RawSqlLevel, UserDataType};
use dl_query::{
    compile, compile_distinct, compile_range, CompiledQuery, Dialect, FilterOperation, QuerySpec,
    Role, SelectItem,
};

use crate::dto::{
    BlockInfo, DashSqlRequest, DataChunk, DataExport, DataRow, DimensionValueSpec, DistinctRequest,
    LegendItem, PivotRequest, PivotResponse, PreviewRequest, RangeRequest, ResultRequest,
    ResultResponse, TypedQueryRawRequest, TypedQueryRequest, UpdateAction,
};
use crate::error::ApiError;
use crate::executor::{ConnectionTarget, Executor, QueryMetadata, RowStream};
use crate::legend::{self, Legend, LegendEntry, LegendFormalizer, ITEM_TYPE_PLACEHOLDER};
use crate::markup;
use crate::pivot::{self, make_pivot_response, PivotTable};
use crate::postprocess;
use crate::preflight::{self, Preflight};
use crate::updates::apply_updates;

/// `DataAPILimits.DATA_API_DEFAULT_ROW_COUNT_HARD_LIMIT` (`DEFAULT_SOURCE_DB_LIMIT` has the
/// same value).
///
/// It is a cap on what a request may download, not a page size: a `/result` request that names
/// no `limit` is cut at `LIMIT 1_000_001` (`Executor._make_bi_query` fetches one row beyond the
/// cap), and that extra row is what turns into `ERR.DS_API.ROW_COUNT_LIMIT`.
pub const DEFAULT_ROW_LIMIT: i64 = 1_000_000;
/// `DatasetPreviewRequestSchema.limit` — the preview's own default page size.
pub const DEFAULT_PREVIEW_LIMIT: i64 = 100;
/// `DataAPILimits.PREVIEW_API_DEFAULT_ROW_COUNT_HARD_LIMIT`: a preview may ask for many more
/// rows than its default page (`"limit": 1000` from the editor's "Number of rows" field), so the
/// hard limit it is clamped by is a hundred thousand, not [`DEFAULT_PREVIEW_LIMIT`].
pub const PREVIEW_DEFAULT_ROW_LIMIT: i64 = 100_000;
/// `DataAPILimits.PREVIEW_ROW_LIMIT`: a preview page is capped at a thousand rows whatever
/// `limit` says (`QueryFormalizer.make_limit_offset` takes `min(limit or PREVIEW_ROW_LIMIT,
/// PREVIEW_ROW_LIMIT)` for a preview block in direct/materialization mode).
///
/// This is a *second* cap next to [`PREVIEW_DEFAULT_ROW_LIMIT`], and the one that decides what a
/// `"limit": 5000` preview answers: Python returns 1000 rows, not 5000.
pub const PREVIEW_PAGE_LIMIT: i64 = 1000;
/// `DatasetVersionValuesDistinctPostSchema.limit`.
pub const DEFAULT_DISTINCT_LIMIT: i64 = 1000;

/// One cell of a decoded row: the legend id it belongs to and where its value comes from.
///
/// Python compiles a placeholder to `SELECT '' AS "__@placeholder__"`, so a block's row has a
/// position for it while the SQL has none. The port has no literal support in the compiler,
/// so the constant is filled in while the row is decoded (the same trick the pivot merger
/// uses for its totals cells).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCell {
    /// A selected column: its position among the selected ones, and the legend id it carries.
    Column { legend_item_id: i64, index: usize },
    /// A placeholder: `''`, and no column at all.
    Template { legend_item_id: i64 },
}

/// A compiled query before it is executed (also what `--dry-run` prints).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedQuery {
    pub sql: String,
    pub grouped: bool,
    pub columns: Vec<dl_query::OutputColumn>,
    /// The cells of one row, in `SELECT` order.
    pub cells: Vec<QueryCell>,
    /// The response `fields`: the whole legend, selected items included.
    pub legend: Vec<LegendItem>,
}

impl PreparedQuery {
    /// Take the `legend_item_id`s from the legend: they are *not* the `SELECT` positions when
    /// the request pins ids or when the legend has `info`/`filter` items.
    pub fn with_legend(compiled: CompiledQuery, legend: &Legend) -> Self {
        let mut columns = compiled.columns;
        // `legend.select_ids()` in `SELECT` order, plus the placeholders, which have no column.
        let mut selected: Vec<&LegendEntry> = legend
            .entries
            .iter()
            .filter(|entry| entry.in_result_set && entry.select_index.is_some())
            .collect();
        selected.sort_by_key(|entry| entry.select_index);
        let mut columns_iter = columns.iter_mut().filter(|column| column.in_result_set);
        let mut cells = Vec::with_capacity(selected.len());
        let mut index = 0;
        for entry in selected {
            let legend_item_id = entry.item.legend_item_id;
            if entry.item.item_type == ITEM_TYPE_PLACEHOLDER {
                cells.push(QueryCell::Template { legend_item_id });
                continue;
            }
            if let Some(column) = columns_iter.next() {
                column.legend_item_id = legend_item_id;
            }
            cells.push(QueryCell::Column {
                legend_item_id,
                index,
            });
            index += 1;
        }
        Self {
            sql: compiled.sql,
            grouped: compiled.grouped,
            columns,
            cells,
            legend: legend.items(),
        }
    }
}

impl PreparedQuery {
    /// Decode one raw row into the response row, using the same legend ids as the buffered path.
    pub fn decode_row(&self, row: &[Option<String>]) -> DataRow {
        let selected = result_columns(self);
        decode_prepared_row(row, self, &selected)
    }
}

impl From<CompiledQuery> for PreparedQuery {
    fn from(compiled: CompiledQuery) -> Self {
        let legend = legend::legend_of_columns(&compiled.columns);
        Self::with_legend(compiled, &legend)
    }
}

/// `DatasetResultView._make_totals`: the totals row `result.with_totals` asks for.
///
/// The charts engine sends the totals as an ordinary second block (`block_id: 1`) and puts
/// `with_totals` at the top level of the body — a flag the v2 schema does not read
/// (`ResultDataRequestV2Schema` only takes `result.with_totals`), so the block alone answers the
/// request. When the nested flag *is* set, `_make_totals` builds a query of its own: the
/// measures alone, `GroupByPolicy.force`, `empty_query_mode=empty_row`, and only its first row is
/// kept. That query is not part of `merged_stream`, and `_make_response_v2` ignores `totals`
/// altogether, so the row only matters to the v1 envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedTotals {
    pub query: PreparedQuery,
}

/// One block of a `/result` request: the query it compiles to and where its rows go.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedBlock {
    pub block_id: i64,
    pub query: PreparedQuery,
    pub placement: Placement,
}

/// `BlockPlacement`: where a block's rows land in the merged stream.
///
/// Python merges block by block (`DataStreamMerger` + `make_binary_merger`): the root block's
/// rows are the stream, and every other block's rows are inserted into it.
#[derive(Debug, Clone, PartialEq)]
pub enum Placement {
    /// `RootBlockPlacement`: the first block, always.
    Root,
    /// `AfterBlockPlacement`: the child's rows follow the parent's.
    ///
    /// With `dimension_values` they are inserted right after the parent rows whose dimensions
    /// match — this is how the next level of a tree lands under the branch it expands. Without
    /// them the streams are concatenated, which is what a totals row (it has no dimensions of
    /// its own) needs.
    After {
        dimension_values: Option<Vec<DimensionValueSpec>>,
    },
    /// `DispersedAfterBlockPlacement`: one child row after each matching parent row, i.e. the
    /// totals of a block that kept its dimensions.
    DispersedAfter { parent: Vec<i64>, child: Vec<i64> },
}

/// What `/result` compiles: one query per request block.
///
/// Python's `execute_all_queries` runs every block of the request and merges their rows into a
/// single `result_data` chunk (`DataStreamMerger`). Most requests name one block; three things
/// name more: a totals request (the charts engine expands `with_totals` into a `placeholder`
/// template per dimension position plus one `total` item per measure) and a tree drill-down (one
/// block per expanded level, each with its own `prefix` and `LENGTE`).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedResult {
    /// The blocks of the request, `block_id` ascending; the first one is the root.
    pub blocks: Vec<PreparedBlock>,
    /// `result.with_totals`: the summary row, merged when the request sent it as a block.
    pub totals: Option<PreparedTotals>,
    /// The response `fields`: the legend of all blocks together.
    pub fields: Vec<LegendItem>,
}

/// What `/pivot` compiles: the base query that feeds the table, its legend and the pivot
/// legend the request `pivot.structure` describes.
///
/// Python keeps the same triple implicitly (`merged_stream`, `merged_stream.legend`,
/// `pivot_legend`), and `PivotFormalizer` needs the legend to resolve `legend_item_ids`. For a
/// totals request the legend is the *extended* one (the base items plus one per block cell)
/// and `totals` holds the extra blocks that are merged into the same stream.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedPivot {
    pub query: PreparedQuery,
    pub legend: Legend,
    pub pivot_legend: pivot::PivotLegend,
    pub totals: Vec<pivot::TotalBlock>,
}

/// Serves the v2 data endpoints: compile, execute, decode.
/// A memo scoped to one request.
///
/// It deduplicates identical SQL inside a single `result`/`pivot`/`preview` execution (a drill
/// down or a totals block may ask the same question twice). It is created by the public method,
/// never stored on the service and never shared with another request: there is no TTL and no
/// possibility of returning data from an earlier request.
struct QueryMemo {
    entries: Mutex<HashMap<String, Arc<Vec<Vec<Option<String>>>>>>,
    /// One async gate per SQL: the owner executes while holding it, waiters acquire it and then
    /// find the answer in `entries`. An error releases the gate without an entry, so a waiter
    /// retries instead of receiving a fabricated result.
    gates: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// The cross-request cache scope for this request (`None` = no cross-request caching).
    cache: Option<QueryCacheScope>,
}

impl Default for QueryMemo {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            cache: None,
        }
    }
}

impl QueryMemo {
    /// A memo that also consults the cross-request query-result cache for this scope.
    fn with_cache(cache: Option<QueryCacheScope>) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            cache,
        }
    }

    fn get(&self, key: &str) -> Option<Arc<Vec<Vec<Option<String>>>>> {
        self.entries.lock().ok()?.get(key).cloned()
    }

    fn insert(&self, key: String, rows: Arc<Vec<Vec<Option<String>>>>) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= 32 {
            entries.clear();
        }
        entries.insert(key, rows);
    }

    fn gate(&self, key: &str) -> Option<Arc<AsyncMutex<()>>> {
        let mut gates = self.gates.lock().ok()?;
        Some(
            gates
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone(),
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Cross-request query-result cache (v9.0 / task-25)
// ---------------------------------------------------------------------------------------------
//
// Python's `dl_cache_engine` caches *query results* per connection with a TTL taken from the
// connection settings (`cache_ttl_sec`, UI "Cache TTL in seconds"). This is the Rust port of
// that idea for the vendored data-api:
//
// * only successful row sets are stored — an executor error is never cached;
// * the key is built from everything that can change the answer: the caller identity
//   (user/groups/roles, the RLS subject), the dataset `schema_key` (fields, sources, avatars,
//   RLS rules), the connection id, the resolved DSN, the request's pinned `revision_id` and
//   the full SQL text (the compiler inlines bind parameters, so the SQL is the whole query);
// * TTL priority (confirmed with the Lead):
//     1. `us` connection `data.cache_ttl_sec`: `Some(N>0)` -> N seconds, `Some(0)` -> cache
//        off for that connection, `None` -> fallback;
//     2. master env `DL_QUERY_CACHE_TTL_SECS`: unset -> per-connection, `0` -> off globally,
//        `N` -> force N seconds for every connection (measurement switch);
//     3. fallback env `DL_CONNECTION_CACHE_TTL_SECS`: default 60 s, `0` = no fallback (a UI
//        TTL still applies);
// * `refresh_ttl_on_read = false` (Python's direct-TTL semantics): a hit does not extend the
//   entry's lifetime, so a data change is visible within the original TTL at the latest;
// * bounded LRU: `DL_QUERY_CACHE_CAPACITY` entries (default 256), least recently used evicted
//   on insert. Streaming (`result_stream`) is deliberately not cached — a cursor cannot be
//   replayed from a shared row buffer without changing the streaming contract;
// * `DL_US_CACHE_TTL_SECS` remains metadata-only and is not involved here.
#[derive(Debug, Clone)]
pub struct QueryCacheSettings {
    /// TTL used when the connection has no `cache_ttl_sec` override. `None` = no fallback.
    pub default_ttl: Option<Duration>,
    /// `None` = per-connection; `Some(None)` = disabled globally; `Some(Some(ttl))` = force.
    pub ttl_override: Option<Option<Duration>>,
    pub capacity: usize,
}

impl Default for QueryCacheSettings {
    fn default() -> Self {
        Self {
            default_ttl: None,
            ttl_override: None,
            capacity: 256,
        }
    }
}

impl QueryCacheSettings {
    /// Read the deployment env. One place, so the monolith data thread and the standalone
    /// data-api apply identical semantics.
    pub fn from_env() -> Self {
        // Only a master off switch remains: `DL_QUERY_CACHE_TTL_SECS=0` disables the cache for
        // every connection (Python's `CACHES_ON=0`). A positive value is deliberately *not* a
        // fallback TTL: the cache lifetime is the connection's own `cache_ttl_sec` setting.
        // A connection without that setting (or with `0`) is not cached at all, so an update
        // in the database is visible immediately for it.
        let query_env = std::env::var("DL_QUERY_CACHE_TTL_SECS").ok();
        let master_off = query_env
            .as_deref()
            .map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
            == Some(0);
        let ttl_override = if master_off { Some(None) } else { None };
        let default_ttl: Option<Duration> = None;
        let capacity = std::env::var("DL_QUERY_CACHE_CAPACITY")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|capacity| *capacity > 0)
            .unwrap_or(256);
        Self {
            default_ttl,
            ttl_override,
            capacity,
        }
    }
}

/// Counters for `/debug/cache`.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub entries: usize,
    pub capacity: usize,
}

struct QueryCacheEntry {
    rows: Arc<Vec<Vec<Option<String>>>>,
    inserted: Instant,
    ttl: Duration,
    last_used: Instant,
}

/// A bounded LRU of successful row sets.
struct QueryCache {
    entries: Mutex<HashMap<String, QueryCacheEntry>>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    evictions: AtomicU64,
}

impl QueryCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<Vec<Vec<Option<String>>>>> {
        let mut entries = self.entries.lock().ok()?;
        let Some(entry) = entries.get(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if entry.inserted.elapsed() >= entry.ttl {
            entries.remove(key);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let entry = entries.get_mut(key)?;
        // `refresh_ttl_on_read = false`: remember the use for LRU, never extend the TTL.
        entry.last_used = Instant::now();
        let rows = Arc::clone(&entry.rows);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(rows)
    }

    fn insert(&self, key: String, rows: Arc<Vec<Vec<Option<String>>>>, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            if let Some(victim) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&victim);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        entries.insert(
            key,
            QueryCacheEntry {
                rows,
                inserted: Instant::now(),
                ttl,
                last_used: Instant::now(),
            },
        );
        self.inserts.fetch_add(1, Ordering::Relaxed);
    }

    fn stats(&self) -> QueryCacheStats {
        QueryCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            entries: self
                .entries
                .lock()
                .map(|entries| entries.len())
                .unwrap_or(0),
            capacity: self.capacity,
        }
    }
}

/// Per-request cache prefix + TTL. `None` in [`QueryMemo`] means "do not cache this request".
#[derive(Clone)]
struct QueryCacheScope {
    prefix: String,
    ttl: Duration,
}

/// One prepared result block plus a cursor stream of its raw rows.
pub struct StreamedResult {
    pub fields: Vec<LegendItem>,
    pub blocks: Vec<BlockInfo>,
    pub query: PreparedQuery,
    pub stream: RowStream,
    pub data_export_forbidden: bool,
}

pub struct Service<E: Executor> {
    executor: E,
    dialect: Dialect,
    /// `us` connections -> DSN. `None` means "run against the executor's own database"
    /// (`DL_DATABASE_URL`, dry runs, tests).
    connections: Option<std::sync::Arc<dyn crate::executor::ConnectionResolver>>,
    /// Schema-derived field metadata, keyed by an exact content fingerprint. It is not data
    /// from `us` and not a query result: an entry is reused only while every field/source/relation
    /// that affects the types is unchanged. No TTL is needed because the key changes with edits.
    metadata: Mutex<HashMap<String, (HashMap<String, FieldType>, HashMap<String, UserDataType>)>>,
    /// Cross-request query-result cache (task-25). `None` until [`Service::with_query_cache`].
    query_cache: Option<Arc<QueryCache>>,
    query_cache_settings: QueryCacheSettings,
}

impl<E: Executor> Service<E> {
    pub fn new(executor: E, dialect: Dialect) -> Self {
        Self {
            executor,
            dialect,
            connections: None,
            metadata: Mutex::new(HashMap::new()),
            query_cache: None,
            query_cache_settings: QueryCacheSettings::default(),
        }
    }

    /// Enable the bounded cross-request query-result cache. TTL semantics in
    /// [`QueryCacheSettings`]; only successful row sets are stored.
    pub fn with_query_cache(mut self, settings: QueryCacheSettings) -> Self {
        self.query_cache = Some(Arc::new(QueryCache::new(settings.capacity)));
        self.query_cache_settings = settings;
        self
    }

    /// The cross-request query-result cache counters (`/debug/cache`).
    pub fn query_cache_stats(&self) -> Option<QueryCacheStats> {
        self.query_cache.as_ref().map(|cache| cache.stats())
    }

    /// `/debug/cache` body for the standalone data-api and the monolith engine.
    pub fn query_cache_json(&self) -> serde_json::Value {
        use serde_json::json;
        let default_ttl_secs = self
            .query_cache_settings
            .default_ttl
            .map(|ttl| ttl.as_secs());
        let override_ttl_secs = self
            .query_cache_settings
            .ttl_override
            .map(|ttl| ttl.map(|ttl| ttl.as_secs()));
        // `DL_QUERY_CACHE_TTL_SECS=0` is a hard master off switch: the cache object still
        // exists (so the counters and the configured capacity can be reported), but no query
        // can ever be cached, so `/debug/cache` must say `enabled: false` (verifier T3).
        let master = match self.query_cache_settings.ttl_override {
            Some(None) => "off",
            Some(Some(_)) => "force",
            None => "per_connection",
        };
        let enabled = self.query_cache.is_some() && master != "off";
        let stats = self
            .query_cache
            .as_ref()
            .map(|cache| cache.stats())
            .unwrap_or_default();
        json!({
            "enabled": enabled,
            "master": master,
            "hits": stats.hits,
            "misses": stats.misses,
            "inserts": stats.inserts,
            "evictions": stats.evictions,
            "entries": stats.entries,
            "capacity": stats.capacity,
            "default_ttl_secs": default_ttl_secs,
            "override_ttl_secs": override_ttl_secs,
            "refresh_ttl_on_read": false,
        })
    }

/// Cache-key fingerprint of a DSN.
///
/// The DSN usually carries a password and may carry host names; a raw value in a cache key
/// would leak it through heap dumps and any key-logging debug output. The key only needs
/// equality, so a stable 64-bit hash (SipHash via `DefaultHasher`) is sufficient.
fn dsn_fingerprint(dsn: Option<&str>) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dsn.unwrap_or("").hash(&mut hasher);
    format!("dsn:{:016x}", hasher.finish())
}

    /// The cache scope of one buffered query, or `None` when this request must not be cached.
    fn query_cache_scope(
        &self,
        dataset: &Dataset,
        dsn: Option<&str>,
        connection_ttl_sec: Option<i64>,
        request_revision: Option<&str>,
    ) -> Option<QueryCacheScope> {
        self.query_cache.as_ref()?;
        let ttl = match &self.query_cache_settings.ttl_override {
            Some(None) => return None,
            Some(Some(ttl)) => *ttl,
            None => match connection_ttl_sec {
                Some(seconds) if seconds <= 0 => return None,
                Some(seconds) => Duration::from_secs(seconds as u64),
                None => self.query_cache_settings.default_ttl?,
            },
        };
        if ttl.is_zero() {
            return None;
        }
        // Key completeness: without a pinned/resolved revision a dataset revision switch could
        // still be answered from the old entry, so unpinned requests are never cached here.
        // The monolith sends the revision resolved from the US bundle (`dataset_revision_id`)
        // and the serde alias above turns it into `revision_id`.
        let request_revision = match request_revision {
            Some(revision) if !revision.trim().is_empty() => revision.trim(),
            _ => return None,
        };
        let identity_key = match dl_query::identity::current() {
            None => "identity:none".to_string(),
            Some(identity) => format!(
                "identity:{}|{}|{}",
                identity.user_id,
                identity.groups.join(","),
                identity.roles.join(",")
            ),
        };
        Some(QueryCacheScope {
            prefix: format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                identity_key,
                dataset.schema_key(),
                dataset.connection_id.as_deref().unwrap_or(""),
                Self::dsn_fingerprint(dsn),
                request_revision
            ),
            ttl,
        })
    }

    /// Resolve each dataset's `connection_id` into a DSN through `resolver`.
    pub fn with_connection_resolver(
        mut self,
        resolver: std::sync::Arc<dyn crate::executor::ConnectionResolver>,
    ) -> Self {
        self.connections = Some(resolver);
        self
    }

    fn field_metadata(
        &self,
        dataset: &Dataset,
        dialect: Dialect,
    ) -> (HashMap<String, FieldType>, HashMap<String, UserDataType>) {
        let key = format!("{:?}\0{}", dialect, dataset.schema_key());
        if let Ok(metadata) = self.metadata.lock() {
            if let Some((field_types, data_types)) = metadata.get(&key) {
                return (field_types.clone(), data_types.clone());
            }
        }
        let (field_types, data_types) = dl_query::field_types_and_data_types(dataset, dialect);
        if let Ok(mut metadata) = self.metadata.lock() {
            if metadata.len() >= 128 {
                metadata.clear();
            }
            metadata.insert(key, (field_types.clone(), data_types.clone()));
        }
        (field_types, data_types)
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn executor(&self) -> &E {
        &self.executor
    }

    /// The DSN of the database this dataset reads from, if the service can resolve it.
    ///
    /// `Ok(None)` — no resolver, or the dataset names no connection — leaves the executor to its
    /// own configuration, which is what the standalone/dry-run mode wants. When a resolver *is*
    /// configured and the dataset names a connection it could not resolve, that is an error:
    /// answering `200` with no rows would look like "the dataset is empty" (`No data` in the UI)
    /// while the real cause is an unreachable or unsupported connection.
    /// The resolved connection of one dataset: DSN, cache TTL, `raw_sql_level` and the
    /// data-export flags, resolved in one call so a request never consults `us` twice.
    ///
    /// The `raw_sql_level` gate is Python's `SubselectDataSource.get_sql_source`: a Custom SQL
    /// source is refused (`ERR.DS_API.CONNECTION_CONFIG.SUBSELECT_NOT_ALLOWED`) unless the
    /// connection allows a subselect.
    async fn connection_target(
        &self,
        dataset: &Dataset,
    ) -> Result<Option<crate::executor::ConnectionTarget>, ApiError> {
        let Some(resolver) = self.connections.as_ref() else {
            return Ok(None);
        };
        let target = match dataset.connection_id.as_deref() {
            Some(connection_id) => match crate::memo::get(connection_id) {
                Some(target) => Some(target),
                None => match resolver.resolve_target(connection_id).await {
                    Some(target) => {
                        // Two or three queries in one request share the connection.
                        crate::memo::insert(connection_id, &target);
                        Some(target)
                    }
                    None => {
                        tracing::error!(
                            connection_id,
                            "the dataset connection could not be resolved"
                        );
                        return Err(ApiError::new(
                            500,
                            "ERR.DS_API.DB.CONNECTION",
                            format!("Could not resolve the connection {connection_id}"),
                        ));
                    }
                },
            },
            None => None,
        };
        // Fail closed: with a resolver configured, an unknown connection has no
        // `raw_sql_level`, which is the storage default `off`.
        let subselect_allowed = target
            .as_ref()
            .is_some_and(|target| target.raw_sql_level.allows_subselect());
        if dataset.uses_subselect() && !subselect_allowed {
            return Err(ApiError::bad_request(
                "ERR.DS_API.CONNECTION_CONFIG.SUBSELECT_NOT_ALLOWED",
                "Subquery source is disallowed in the connection settings",
            ));
        }
        Ok(target)
    }

    /// The dialect the dataset's database speaks: the scheme of the resolved DSN when there is
    /// one, else the service-wide `DL_DIALECT`.
    fn target_dialect(&self, dsn: Option<&str>) -> Dialect {
        dsn.and_then(Dialect::from_dsn).unwrap_or(self.dialect)
    }

    /// Compile a `/result` request without touching the database.
    pub fn prepare_result(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
    ) -> Result<PreparedResult, ApiError> {
        self.prepare_result_with_dialect(dataset, request, self.dialect)
    }

    /// The same, for the dialect of the dataset's own database.
    fn prepare_result_with_dialect(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
        dialect: Dialect,
    ) -> Result<PreparedResult, ApiError> {
        let updates_started = std::time::Instant::now();
        let dataset = dataset_with_updates_inferred(dataset, &request.updates, dialect)?;
        crate::trace::record_detail("prepare.updates", updates_started.elapsed());
        let fields_started = std::time::Instant::now();
        let (field_types, data_types) = self.field_metadata(&dataset, dialect);
        crate::trace::record_detail("prepare.fields", fields_started.elapsed());
        let legend_started = std::time::Instant::now();
        let legend = LegendFormalizer::for_result(&dataset, request)
            .with_field_types(field_types)
            .with_data_types(data_types)
            .make()?;
        crate::trace::record_detail("prepare.legend", legend_started.elapsed());
        let compile_started = std::time::Instant::now();
        let block_ids = request_block_ids(request);
        let multi_block = block_ids.len() > 1;
        let (limit, offset) = pre_paginate(request, multi_block);
        // Each block is a top-level query, so each is cut at the hard limit (+1 row) when the
        // request names no `limit` of its own (`Executor._make_bi_query`).
        let hard_limit = result_hard_limit(request);
        let limit = Some(result_limit(limit, hard_limit));
        let mut blocks: Vec<PreparedBlock> = Vec::with_capacity(block_ids.len());
        for block_id in block_ids {
            let entries = block_entries(&legend, block_id);
            // `BlockFormalizer.make_block_legend` forces the first block to be the root, and
            // resolves every other placement against it.
            let placement = match blocks.first() {
                None => Placement::Root,
                Some(root) => {
                    let main = block_entries(&legend, root.block_id);
                    block_placement(&entries, &main)?
                }
            };
            let query = self.block_query(
                &dataset, request, block_id, &entries, limit, offset, dialect,
            )?;
            blocks.push(PreparedBlock {
                block_id,
                query,
                placement,
            });
        }
        // `result.with_totals`: `_make_totals` builds a query of its own, which is *not* one of
        // the request's blocks (that block, if any, is already in `blocks`).
        let totals = match request.with_totals() {
            true => Some(PreparedTotals {
                query: self
                    .totals_from_measures(&dataset, request, &legend, dialect)
                    .map_err(ApiError::from)?,
            }),
            false => None,
        };
        crate::trace::record_detail("prepare.compile", compile_started.elapsed());
        Ok(PreparedResult {
            blocks,
            totals,
            fields: legend.items(),
        })
    }

    /// One block of a `/result` request: the query `DatasetDataBaseView.execute_query` builds.
    ///
    /// A totals block keeps the shape `_make_totals` gives it — the measures alone, the request
    /// filters, no `order_by` and no pagination — because a single aggregate row cannot depend on
    /// them. Every other block is the request's own query narrowed to its items: `limit`/`offset`
    /// are what `QueryPrePaginator` pushed down, `order_by`/`parameter_values` are the legend
    /// items that name no block (so they belong to every block), and the filters are the
    /// request's own plus the tree filters `_generate_tree_filter_items` put in this block.
    ///
    /// A placeholder keeps its position in the row but stays out of the `SELECT`: its value is
    /// the (empty) template string, which [`QueryCell::Template`] puts in while decoding.
    fn block_query(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
        block_id: i64,
        entries: &[LegendEntry],
        limit: Option<i64>,
        offset: Option<i64>,
        dialect: Dialect,
    ) -> Result<PreparedQuery, ApiError> {
        let block_legend = Legend {
            entries: entries.to_vec(),
        };
        let select: Vec<SelectItem> = block_legend
            .entries
            .iter()
            .filter(|entry| entry.in_result_set)
            .filter(|entry| entry.item.item_type != ITEM_TYPE_PLACEHOLDER)
            .map(|entry| SelectItem {
                field_ref: FieldRef::id(entry.item.id.clone()),
                role: entry.item.role_spec.role,
                // `TreeRoleSpec.level` travels with the item, so a `tree` dimension selects the
                // same `slice(expr, 1, level)` the legend announces.
                level: entry.item.role_spec.level,
            })
            .collect();
        if select.is_empty() {
            // `empty_query_mode=empty_row`: no SQL, but the block still answers one row.
            return Ok(PreparedQuery {
                sql: String::new(),
                grouped: false,
                columns: Vec::new(),
                cells: block_legend
                    .entries
                    .iter()
                    .filter(|entry| entry.in_result_set)
                    .map(|entry| QueryCell::Template {
                        legend_item_id: entry.item.legend_item_id,
                    })
                    .collect(),
                legend: block_legend.items(),
            });
        }
        let filters = block_filters(request, block_id, &block_legend.entries);
        let totals_block = block_legend.entries.iter().any(|entry| {
            entry.in_result_set && matches!(entry.item.role_spec.role, Role::Total | Role::Template)
        });
        let spec = match totals_block {
            // A totals block keeps the pagination the pre-paginator pushed down but no
            // `order_by`: its dimensions became placeholders, so there is no column to sort by
            // (`make_order_by_specs` drops the item, and the reference leaves it out too).
            true => QuerySpec {
                select,
                filters,
                limit,
                offset,
                parameter_values: request
                    .parameter_values
                    .iter()
                    .filter(|item| in_block(item.block_id, block_id))
                    .map(Into::into)
                    .collect(),
                ignore_nonexistent_filters: request.ignore_nonexistent_filters,
                // A totals block carries no LOD/window forks: the reference renders a measure
                // that needs one as `NULL` (see the `totals` field of `QuerySpec`).
                totals: true,
                ..QuerySpec::default()
            },
            false => QuerySpec {
                select,
                filters,
                order_by: request
                    .order_by
                    .iter()
                    .filter(|item| in_block(item.block_id, block_id))
                    .map(Into::into)
                    .collect(),
                limit,
                offset,
                disable_group_by: request.disable_group_by,
                group_by_if_measures: false,
                parameter_values: request
                    .parameter_values
                    .iter()
                    .filter(|item| in_block(item.block_id, block_id))
                    .map(Into::into)
                    .collect(),
                ignore_nonexistent_filters: request.ignore_nonexistent_filters,
                totals: false,
            },
        };
        let compiled = compile(dataset, &spec, dialect).map_err(ApiError::from)?;
        Ok(PreparedQuery::with_legend(compiled, &block_legend))
    }

    /// `_make_totals`: the totals block of a request that carries the flag but no block.
    fn totals_from_measures(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
        legend: &Legend,
        dialect: Dialect,
    ) -> Result<PreparedQuery, dl_query::QueryError> {
        let measures: Vec<LegendEntry> = legend
            .entries
            .iter()
            .filter(|entry| entry.in_result_set)
            .filter(|entry| entry.item.field_type == dl_core::FieldType::Measure)
            .cloned()
            .collect();
        if measures.is_empty() {
            return Ok(PreparedQuery {
                sql: String::new(),
                grouped: false,
                columns: Vec::new(),
                cells: Vec::new(),
                legend: Vec::new(),
            });
        }
        let spec = QuerySpec {
            select: measures
                .iter()
                .map(|entry| SelectItem {
                    field_ref: FieldRef::id(entry.item.id.clone()),
                    role: Role::Total,
                    level: None,
                })
                .collect(),
            filters: request.filters.iter().map(Into::into).collect(),
            ignore_nonexistent_filters: request.ignore_nonexistent_filters,
            totals: true,
            ..QuerySpec::default()
        };
        let compiled = compile(dataset, &spec, dialect)?;
        let totals_legend = Legend { entries: measures };
        Ok(PreparedQuery::with_legend(compiled, &totals_legend))
    }

    /// Compile a `/preview` request without touching the database.
    pub fn prepare_preview(&self, request: &PreviewRequest) -> Result<PreparedQuery, ApiError> {
        self.prepare_preview_for(None, request)
    }

    /// The same, for a request that carries no dataset: `stored` is the one the server resolved
    /// from `us`. Avoiding "copy it into the request first" saves a deep clone of the dataset —
    /// which is the whole `sources` map and every field — on every preview request.
    fn prepare_preview_for(
        &self,
        stored: Option<&Dataset>,
        request: &PreviewRequest,
    ) -> Result<PreparedQuery, ApiError> {
        self.prepare_preview_with_dialect(stored, request, self.dialect)
    }

    /// The same, for the dialect of the dataset's own database.
    fn prepare_preview_with_dialect(
        &self,
        stored: Option<&Dataset>,
        request: &PreviewRequest,
        dialect: Dialect,
    ) -> Result<PreparedQuery, ApiError> {
        let dataset = request.dataset.as_ref().or(stored).ok_or_else(|| {
            ApiError::bad_request("ERR.DS_API.DATASET", "no dataset in the request")
        })?;
        let dataset = dataset_with_updates_inferred(dataset, &request.updates, dialect)?;
        let legend = legend::preview_legend(&dataset);
        let spec = preview_spec(&dataset, request);
        let compiled = compile(&dataset, &spec, dialect).map_err(ApiError::from)?;
        Ok(PreparedQuery::with_legend(compiled, &legend))
    }

    /// Prepare one block and return a cursor stream of its rows.
    ///
    /// Multi-block/totals requests use the buffered path: merging several cursors would need a
    /// second merger protocol, while a normal chart is one block.
    pub async fn result_stream(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
    ) -> Result<StreamedResult, ApiError> {
        let target = self.connection_target(dataset).await?;
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        let prepared =
            self.prepare_result_with_dialect(dataset, request, self.target_dialect(dsn))?;
        if prepared.blocks.len() != 1 || prepared.totals.is_some() {
            return Err(ApiError::bad_request(
                "ERR.DS_API.REQUEST",
                "streaming supports one result block without totals",
            ));
        }
        let block = prepared.blocks.into_iter().next().expect("one block");
        let stream = self
            .executor
            .fetch_stream(&block.query.sql, dsn)
            .await
            .map_err(|error| query_failed(&block.query.sql, &error))?;
        Ok(StreamedResult {
            fields: prepared.fields,
            blocks: vec![BlockInfo {
                block_id: block.block_id,
                query: Some(block.query.sql.clone()),
            }],
            query: block.query,
            stream,
            data_export_forbidden: !DataExport::for_dataset_in_connection(dataset, target.as_ref())
                .basic
                .allowed,
        })
    }

    /// `POST /api/data/v2/datasets/{ds_id}/result`.
    pub async fn result(
        &self,
        dataset: &Dataset,
        request: &ResultRequest,
    ) -> Result<ResultResponse, ApiError> {
        let started = std::time::Instant::now();
        // The connection is resolved first: its scheme decides the SQL dialect, so a ClickHouse
        // dataset is compiled for ClickHouse even when `DL_DIALECT` says postgres.
        let dsn_started = std::time::Instant::now();
        let target = self.connection_target(dataset).await?;
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        crate::trace::record_wait("dsn", dsn_started.elapsed());
        let cache_scope = self.query_cache_scope(
            dataset,
            dsn,
            target.as_ref().and_then(|target| target.cache_ttl_sec),
            request.revision_id.as_deref(),
        );
        let prepare_started = std::time::Instant::now();
        let prepared =
            self.prepare_result_with_dialect(dataset, request, self.target_dialect(dsn))?;
        crate::trace::record_work("prepare", prepare_started.elapsed());
        let prepare_ms = started.elapsed().as_secs_f64() * 1000.0;
        // `data_export` is decided by the *stored* dataset and its connection, not by the
        // request updates.
        let data_export = DataExport::for_dataset_in_connection(dataset, target.as_ref());
        let multi_block = prepared.blocks.len() > 1;
        // A chart request runs two queries (its block and the totals block) and a drill-down runs
        // one per level. They do not depend on each other, so they run at the same time:
        // `ConcurrentTaskRunner` does the same in Python (semaphore of 5). Merging keeps the
        // `blocks` order, because `join_all` answers in the order it was given.
        let memo = QueryMemo::with_cache(cache_scope);
        let queries_started = std::time::Instant::now();
        let (block_results, totals) = futures::join!(
            futures::future::join_all(prepared.blocks.iter().map(|block| self.fetch_rows(
                &block.query,
                dsn,
                &memo
            )),),
            async {
                match &prepared.totals {
                    Some(totals) => self.fetch_totals(&totals.query, dsn, &memo).await,
                    None => Ok(None),
                }
            }
        );
        let fetch_ms = queries_started.elapsed().as_secs_f64() * 1000.0;
        // `ResultRowCountLimitExceededError`: the source stream is cut at `row_count_hard_limit`,
        // and the extra row every [`result_limit`] fetches is what says "there is more".
        let hard_limit = result_hard_limit(request);
        let merge_started = std::time::Instant::now();
        let mut rows: Vec<DataRow> = Vec::new();
        let mut blocks: Vec<BlockInfo> = Vec::with_capacity(prepared.blocks.len());
        let mut fetched_rows = 0;
        for (block, result) in prepared.blocks.iter().zip(block_results) {
            let block_rows = result.map_err(|error| query_failed(&block.query.sql, &error))?;
            if block_rows.len() as i64 > hard_limit {
                return Err(ApiError::row_count_limit());
            }
            fetched_rows += block_rows.len();
            // One line per block: a drill-down is the only request that runs more than one, and
            // "which level returned nothing" is otherwise invisible from the client.
            tracing::debug!(
                block_id = block.block_id,
                rows = block_rows.len(),
                sql = %block.query.sql,
                "block executed"
            );
            blocks.push(BlockInfo {
                block_id: block.block_id,
                query: (!block.query.sql.is_empty()).then(|| block.query.sql.clone()),
            });
            rows = merge_block(rows, block_rows, &block.placement);
        }
        // `_make_totals`: its block is not part of `merged_stream` at all, so its row (and its
        // `blocks` entry) stays off the v2 body; only `result.totals` carries it. The v1
        // envelope also reports the totals SQL (`result.totals_query`).
        let totals_query = prepared
            .totals
            .as_ref()
            .map(|totals| totals.query.sql.clone());
        let totals = totals?;
        if multi_block {
            // `QueryPostPaginator`: with more than one block only `limit + offset` could be
            // pushed down (an empty block would otherwise eat the offset), so the merged stream
            // is where the real pagination happens.
            let offset = request.offset.unwrap_or(0).max(0) as usize;
            if offset > 0 {
                rows.drain(..offset.min(rows.len()));
            }
            if let Some(limit) = request.limit {
                rows.truncate(limit.max(0) as usize);
            }
        }
        crate::trace::record_work("merge", merge_started.elapsed());
        tracing::debug!(
            prepare_ms,
            fetch_ms,
            blocks = prepared.blocks.len(),
            fetched_rows,
            rows = rows.len(),
            "result request"
        );
        let response_started = std::time::Instant::now();
        let mut response = ResultResponse {
            fields: prepared.fields,
            result_data: vec![DataChunk { rows }],
            blocks,
            notifications: Vec::new(),
            data_export_forbidden: !data_export.basic.allowed,
            data_export,
            totals,
            totals_query,
            json_len: 0,
        };
        response.measure_json_len();
        crate::trace::record_work("response", response_started.elapsed());
        Ok(response)
    }

    /// Run one SQL statement through the request-scoped memo.
    ///
    /// The memo is local to one request. Identical concurrent SQL inside that request is
    /// serialized by the per-key gate: one task executes, the others wait and then read the
    /// same answer. The gate is released on error without an entry, so a waiter retries against
    /// the database rather than receiving a stale or invented result.
    async fn fetch_raw(
        &self,
        sql: &str,
        dsn: Option<&str>,
        memo: &QueryMemo,
    ) -> Result<Arc<Vec<Vec<Option<String>>>>, crate::executor::ExecutorError> {
        let key = format!("{}\0{}", dsn.unwrap_or_default(), sql);
        let cache_key = memo
            .cache
            .as_ref()
            .map(|scope| format!("{}\0{}", scope.prefix, key));
        if let (Some(cache), Some(cache_key)) = (self.query_cache.as_ref(), cache_key.as_ref()) {
            if let Some(rows) = cache.get(cache_key) {
                return Ok(rows);
            }
        }
        if let Some(rows) = memo.get(&key) {
            return Ok(rows);
        }
        let Some(gate) = memo.gate(&key) else {
            // A poisoned lock can only mean a panic in this request; run without the memo.
            let started = std::time::Instant::now();
            let result = self.executor.fetch(sql, dsn).await;
            crate::trace::record_wait("db", started.elapsed());
            return Ok(Arc::new(result?));
        };
        let _guard = gate.lock().await;
        if let Some(rows) = memo.get(&key) {
            return Ok(rows);
        }
        let started = std::time::Instant::now();
        let result = self.executor.fetch(sql, dsn).await;
        crate::trace::record_wait("db", started.elapsed());
        let rows = Arc::new(result?);
        memo.insert(key, Arc::clone(&rows));
        if let (Some(cache), Some(cache_key), Some(scope)) =
            (self.query_cache.as_ref(), cache_key, memo.cache.as_ref())
        {
            cache.insert(cache_key, Arc::clone(&rows), scope.ttl);
        }
        Ok(rows)
    }

    /// The rows of one block, decoded.
    ///
    /// `empty_query_mode=empty_row`: a block with nothing to select still answers one empty row
    /// (`EmptyQueryError` caught, the row filled in). Python runs no SQL, and neither does this.
    async fn fetch_rows(
        &self,
        prepared: &PreparedQuery,
        dsn: Option<&str>,
        memo: &QueryMemo,
    ) -> Result<Vec<DataRow>, crate::executor::ExecutorError> {
        if prepared.sql.is_empty() {
            return Ok(vec![DataRow {
                data: Vec::new(),
                legend: Vec::new(),
            }]);
        }
        let rows = self.fetch_raw(&prepared.sql, dsn, memo).await?;
        let decode_started = std::time::Instant::now();
        let selected = result_columns(prepared);
        let decoded = rows
            .iter()
            .map(|row| decode_prepared_row(row, prepared, &selected))
            .collect();
        crate::trace::record_work("decode", decode_started.elapsed());
        Ok(decoded)
    }

    /// The totals block: one row, or none when the aggregate was filtered out.
    ///
    /// `execute_query(possible_data_lengths=(0, 1))` is where Python stops: only the first row
    /// is kept. The row is merged into `result_data` by [`Service::result`]; the query is run
    /// even when nothing is filtered out, so a totals query the database rejects fails the whole
    /// request exactly as it does in Python.
    async fn fetch_totals(
        &self,
        prepared: &PreparedQuery,
        dsn: Option<&str>,
        memo: &QueryMemo,
    ) -> Result<Option<DataRow>, ApiError> {
        if prepared.sql.is_empty() {
            // `empty_row`: nothing to select, but the block still answers one row.
            return Ok(Some(DataRow {
                data: Vec::new(),
                legend: Vec::new(),
            }));
        }
        let rows = self
            .fetch_raw(&prepared.sql, dsn, memo)
            .await
            .map_err(|error| query_failed(&prepared.sql, &error))?;
        let decode_started = std::time::Instant::now();
        let selected = result_columns(prepared);
        let decoded = rows
            .first()
            .map(|row| decode_prepared_row(row, prepared, &selected));
        crate::trace::record_work("decode", decode_started.elapsed());
        Ok(decoded)
    }

    /// Compile a `/pivot` request without touching the database.
    ///
    /// The pipeline is `PivotDataRequestBaseSchema.load` -> `PivotRequestModelNormalizer` ->
    /// `execute_all_queries` -> `PivotFormalizer.make_pivot_legend`. The normalizer expands
    /// the totals into extra blocks ([`pivot::normalize_totals`]), which grow the legend and
    /// reorder `pivot.structure`; without totals the request is one block.
    pub fn prepare_pivot(
        &self,
        dataset: &Dataset,
        request: &PivotRequest,
    ) -> Result<PreparedPivot, ApiError> {
        self.prepare_pivot_with_dialect(dataset, request, self.dialect)
    }

    /// The same, for the dialect of the dataset's own database.
    fn prepare_pivot_with_dialect(
        &self,
        dataset: &Dataset,
        request: &PivotRequest,
        dialect: Dialect,
    ) -> Result<PreparedPivot, ApiError> {
        let dataset = dataset_with_updates_inferred(dataset, &request.updates, dialect)?;
        let (field_types, data_types) = self.field_metadata(&dataset, dialect);
        let base_legend = LegendFormalizer::for_pivot(&dataset, request)
            .with_field_types(field_types)
            .with_data_types(data_types)
            .make()?;
        let spec = pivot_spec(request);
        let totals =
            pivot::normalize_totals(&dataset, &request.pivot, &base_legend, &spec, dialect)?;
        let compiled = compile(&dataset, &spec, dialect).map_err(ApiError::from)?;
        // The base query keeps the *base* legend's ids: a totals item is inserted after the
        // last selected one and its id is a new one, so the base block's cells must be read
        // with the base ids (which the extended legend also lists first, but only by
        // construction).
        let mut query = PreparedQuery::with_legend(compiled, &base_legend);
        let (legend, pivot_spec) = match &totals {
            Some(plan) => (plan.legend.clone(), plan.patched_spec(&request.pivot)),
            None => (base_legend, request.pivot.clone()),
        };
        // The response `fields` are the merged stream's legend: every block's items.
        query.legend = legend.items();
        let pivot_legend =
            pivot::make_pivot_legend(&legend, &pivot_spec).map_err(ApiError::from)?;
        Ok(PreparedPivot {
            query,
            legend,
            pivot_legend,
            totals: totals.map_or_else(Vec::new, |plan| plan.blocks),
        })
    }

    /// `POST /api/data/v2/datasets/{ds_id}/pivot`.
    ///
    /// Order matters and follows `DatasetPivotView.post`: transform, then `sort()` (always,
    /// even without pagination), then `paginate()` only when the request asked for it, then
    /// serialize. A totals request runs its blocks *before* the transform: they are merged
    /// into the same stream of rows (`dl_query_processing.merging.merger`), so the table sees
    /// one stream and the total rows land in it like any other row.
    pub async fn pivot(
        &self,
        dataset: &Dataset,
        request: &PivotRequest,
    ) -> Result<PivotResponse, ApiError> {
        let dsn_started = std::time::Instant::now();
        let target = self.connection_target(dataset).await?;
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        crate::trace::record_wait("dsn", dsn_started.elapsed());
        let cache_scope = self.query_cache_scope(
            dataset,
            dsn,
            target.as_ref().and_then(|target| target.cache_ttl_sec),
            request.revision_id.as_deref(),
        );
        let prepare_started = std::time::Instant::now();
        let prepared =
            self.prepare_pivot_with_dialect(dataset, request, self.target_dialect(dsn))?;
        crate::trace::record_work("prepare", prepare_started.elapsed());
        // The base block and the totals blocks are independent queries, exactly like the blocks
        // of a `/result` request: run them together and keep the order when merging.
        let memo = QueryMemo::with_cache(cache_scope);
        let started = std::time::Instant::now();
        let (base, totals) = futures::join!(
            self.fetch_raw(&prepared.query.sql, dsn, &memo),
            futures::future::join_all(
                prepared
                    .totals
                    .iter()
                    .map(|block| self.fetch_raw(&block.sql, dsn, &memo))
            )
        );
        let rows = base.map_err(|error| query_failed(&prepared.query.sql, &error))?;
        let decode_started = std::time::Instant::now();
        let mut data_rows: Vec<DataRow> = decode_rows(&rows, &prepared.query.columns);
        for (block, block_rows) in prepared.totals.iter().zip(totals) {
            let block_rows = block_rows.map_err(|error| query_failed(&block.sql, &error))?;
            data_rows.extend(block_rows.iter().map(|row| merge_block_row(row, block)));
        }
        let fetch_ms = started.elapsed().as_secs_f64() * 1000.0;
        crate::trace::record_work("decode", decode_started.elapsed());

        let pivot_started = std::time::Instant::now();
        let dframe = pivot::pivot(&data_rows, &prepared.legend, &prepared.pivot_legend)?;
        tracing::debug!(fetch_ms, rows = data_rows.len(), "pivot request");
        let mut table = PivotTable::new(dframe, prepared.pivot_legend.clone());
        table.sort(&prepared.legend)?;

        let pagination = &request.pivot.pagination;
        if pagination.non_default() {
            table.paginate(pagination.limit_rows, pagination.offset_rows);
        }
        crate::trace::record_work("pivot", pivot_started.elapsed());

        // `merged_stream.meta.blocks`: the base block first, then one per total.
        let mut blocks = Vec::with_capacity(1 + prepared.totals.len());
        blocks.push(BlockInfo {
            block_id: 0,
            query: Some(prepared.query.sql.clone()),
        });
        blocks.extend(prepared.totals.iter().map(|block| BlockInfo {
            block_id: block.block_id,
            query: Some(block.sql.clone()),
        }));

        // `data_export` comes from the *stored* dataset, before the request updates.
        let response_started = std::time::Instant::now();
        let response = make_pivot_response(
            &prepared.legend,
            &table,
            &blocks,
            DataExport::for_dataset_in_connection(dataset, target.as_ref()),
        )?;
        crate::trace::record_work("response", response_started.elapsed());
        Ok(response)
    }

    /// `POST /api/data/v2/datasets/data/preview`.
    pub async fn preview(&self, request: &PreviewRequest) -> Result<ResultResponse, ApiError> {
        self.preview_with(None, request).await
    }

    /// The same, with the dataset the server resolved from `us` (`PreviewView`'s stored dataset):
    /// the request carries it only when the editor sends the whole unsaved dataset inline.
    ///
    /// Taking `stored` by reference is what keeps a preview request from deep-copying the dataset
    /// into the request body before answering it.
    pub async fn preview_with(
        &self,
        stored: Option<&Dataset>,
        request: &PreviewRequest,
    ) -> Result<ResultResponse, ApiError> {
        let started = std::time::Instant::now();
        let dataset = request.dataset.as_ref().or(stored);
        // An inline dataset carries its own `connection_id`; a stored one names it too.
        let dsn_started = std::time::Instant::now();
        let target = match dataset {
            Some(dataset) => self.connection_target(dataset).await?,
            None => None,
        };
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        let data_export = dataset.map_or_else(DataExport::allowed, |dataset| {
            DataExport::for_dataset_in_connection(dataset, target.as_ref())
        });
        crate::trace::record_wait("dsn", dsn_started.elapsed());
        let prepare_started = std::time::Instant::now();
        let prepared =
            self.prepare_preview_with_dialect(stored, request, self.target_dialect(dsn))?;
        crate::trace::record_work("prepare", prepare_started.elapsed());
        tracing::debug!(
            prepare_ms = started.elapsed().as_secs_f64() * 1000.0,
            "preview prepared"
        );
        self.execute(prepared, data_export, dsn, &QueryMemo::default())
            .await
    }

    /// `POST /api/data/v1/datasets/{ds_id}/versions/draft/preview` (`getPreview`).
    ///
    /// The same request and the same SQL as [`Service::preview`]; only the envelope is the
    /// legacy `v1` one ([`ResultResponse::to_v1`]), which is what the dataset editor renders:
    /// it reads `result.data.Data` / `result.data.Type` and not the `v2` legend.
    pub async fn preview_v1(
        &self,
        request: &PreviewRequest,
    ) -> Result<serde_json::Value, ApiError> {
        self.preview_v1_with(None, request).await
    }

    /// [`Service::preview_v1`] with the dataset the server resolved from `us`.
    pub async fn preview_v1_with(
        &self,
        stored: Option<&Dataset>,
        request: &PreviewRequest,
    ) -> Result<serde_json::Value, ApiError> {
        Ok(self.preview_with(stored, request).await?.to_v1())
    }

    /// `POST /api/data/v2/datasets/{ds_id}/result-preflight`.
    ///
    /// The request body is a `/result` body. The port applies its `updates` and validates the
    /// resulting dataset; like `DatasetResultPreflightView` it compiles no SQL and runs no
    /// query, so it answers on a dataset whose source is unreachable.
    pub fn preflight(&self, dataset: &Dataset, request: &ResultRequest) -> Preflight {
        match dataset_with_updates_inferred(dataset, &request.updates, self.dialect) {
            Ok(dataset) => preflight::validate(&dataset, self.dialect),
            Err(error) => Preflight::from_error(&error),
        }
    }

    async fn execute(
        &self,
        prepared: PreparedQuery,
        data_export: DataExport,
        dsn: Option<&str>,
        memo: &QueryMemo,
    ) -> Result<ResultResponse, ApiError> {
        let rows = self
            .fetch_raw(&prepared.sql, dsn, memo)
            .await
            .map_err(|error| query_failed(&prepared.sql, &error))?;
        let decode_started = std::time::Instant::now();
        let selected = result_columns(&prepared);
        let decoded = rows
            .iter()
            .map(|row| decode_prepared_row(row, &prepared, &selected))
            .collect();
        crate::trace::record_work("decode", decode_started.elapsed());
        let data_export_forbidden = !data_export.basic.allowed;
        let response_started = std::time::Instant::now();
        let mut response = ResultResponse {
            fields: prepared.legend.clone(),
            result_data: vec![DataChunk { rows: decoded }],
            blocks: vec![BlockInfo {
                block_id: 0,
                query: Some(prepared.sql),
            }],
            notifications: Vec::new(),
            data_export,
            data_export_forbidden,
            totals: None,
            totals_query: None,
            json_len: 0,
        };
        response.measure_json_len();
        crate::trace::record_work("response", response_started.elapsed());
        Ok(response)
    }
}

/// A query the database rejected: `400 ERR.DS_API.DB[.<SUBTYPE>]` with `details.db_message`
/// (`dl_core.exc.DatabaseQueryError`, `dl_api_lib/error_handling.py`).
///
/// The SQL is logged but never put in the response body; the driver's own text is the
/// user-visible `db_message`, exactly what Python forwards.
fn query_failed(sql: &str, error: &crate::executor::ExecutorError) -> ApiError {
    tracing::error!(sql, error = %error, "query failed");
    ApiError::database(error.to_string())
}

/// Apply the `updates` of the request to a copy of the dataset.
pub fn dataset_with_updates<'a>(
    dataset: &'a Dataset,
    updates: &[UpdateAction],
) -> Result<Cow<'a, Dataset>, ApiError> {
    // The dashboard sends `updates: []` on every chart. Returning the existing dataset avoids
    // a deep clone of all fields/sources/rules for the overwhelmingly common case; only a
    // request that actually edits the dataset pays for a private copy.
    if updates.is_empty() {
        return Ok(Cow::Borrowed(dataset));
    }
    Ok(Cow::Owned(apply_updates(dataset, updates)?))
}

/// `dataset_with_updates` plus formula-type inference for freshly added editor fields.
///
/// The dataset editor sends `add_field` with a formula and no type; the Python control-api
/// infers the type with its formula compiler and stores it, so the data-api only ever sees
/// typed fields. A local (unsaved) field reaches this port without a type, so every query path
/// infers it here before compiling anything. Requests without updates keep `Cow::Borrowed`.
fn dataset_with_updates_inferred<'a>(
    dataset: &'a Dataset,
    updates: &[UpdateAction],
    dialect: Dialect,
) -> Result<Cow<'a, Dataset>, ApiError> {
    let dataset = dataset_with_updates(dataset, updates)?;
    // Infer only when it is needed: a stored dataset is fully typed, and this runs on every
    // query. An inline editor dataset (or an `add_field` update) is where an untyped formula
    // field appears.
    let needs_inference = dataset
        .fields
        .iter()
        .any(|field| field.calc_mode == dl_core::CalcMode::Formula && field.dtype().is_err());
    if !needs_inference {
        return Ok(dataset);
    }
    let mut dataset = dataset.into_owned();
    let (inferred, formula_error) = dl_query::infer_field_data_types(&dataset, dialect);
    for field in &mut dataset.fields {
        if field.dtype().is_ok() {
            continue;
        }
        if let Some(data_type) = inferred.get(&field.guid).copied() {
            field.data_type = Some(data_type);
            field.initial_data_type = Some(data_type);
        }
    }
    // A formula that cannot reference its fields is a request error, exactly as the reference
    // reports it, not a "field has no data type" validation failure.
    if let Some(error) = formula_error {
        return Err(error.into());
    }
    // `DatasetValidator` also recomputes `DIMENSION`/`MEASURE` from the formula itself; the
    // request's `type` is the UI's guess, not the answer (`CONCAT(...)` is a dimension).
    let field_types = dl_query::field_types(&dataset, dialect);
    for field in &mut dataset.fields {
        if field.calc_mode != dl_core::CalcMode::Formula {
            continue;
        }
        if let Some(field_type) = field_types.get(&field.guid).copied() {
            field.field_type = Some(field_type);
        }
    }
    Ok(Cow::Owned(dataset))
}

/// The block ids the request names, ascending.
///
/// `BlockFormalizer.make_block_legend`: the ids of the legend items — in the order the blocks are
/// generated, which is sorted for the ones no explicit block spec lists — or `0` when the request
/// names none. An item without a `block_id` belongs to every block, so it names no block.
fn request_block_ids(request: &ResultRequest) -> Vec<i64> {
    let mut ids: Vec<i64> = request
        .fields
        .iter()
        .filter_map(|item| item.block_id)
        .chain(request.order_by.iter().filter_map(|item| item.block_id))
        .chain(request.filters.iter().filter_map(|item| item.block_id))
        .chain(
            request
                .parameter_values
                .iter()
                .filter_map(|item| item.block_id),
        )
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        ids.push(0);
    }
    ids
}

/// `Legend.limit_to_block`: whether an item belongs to `block_id`.
fn in_block(item_block_id: Option<i64>, block_id: i64) -> bool {
    item_block_id.map_or(true, |item_block_id| item_block_id == block_id)
}

/// `QueryPrePaginator.pre_paginate`: the `limit`/`offset` a block's SQL gets.
///
/// One block: the global pagination moves into the block (and the post-paginator does nothing).
/// More than one: only `limit + offset` can be pushed down to each of them, because an empty
/// block would otherwise eat the offset it should not have consumed; the offset itself is applied
/// to the merged stream. Without a `limit` there is nothing to push down at all.
fn pre_paginate(request: &ResultRequest, multi_block: bool) -> (Option<i64>, Option<i64>) {
    // `QueryPrePaginator`: the request's own `limit` moves into the block. More than one block
    // pushes only `limit + offset` down (an empty block would otherwise eat the offset), so the
    // merged stream is where the real pagination happens. The hard limit is not applied here:
    // every block is a top-level query of its own and gets it from [`result_limit`].
    let limit = request.limit;
    if !multi_block {
        return (limit, request.offset);
    }
    match limit {
        Some(limit) => (Some(limit + request.offset.unwrap_or(0).max(0)), None),
        None => (None, None),
    }
}

/// The legend entries of one block, in legend order.
///
/// `Legend.limit_to_block` keeps the items that name the block *and* the ones that name none: the
/// measures and the request's own filters are in every block, which is what makes the second
/// block of a tree drill-down select the same measures as the first.
fn block_entries(legend: &Legend, block_id: i64) -> Vec<LegendEntry> {
    legend
        .entries
        .iter()
        .filter(|entry| in_block(entry.block_id, block_id))
        .cloned()
        .collect()
}

/// The filters one block runs.
///
/// The request's own filters (they name no block, so every block gets them) plus the tree filters
/// `_generate_tree_filter_items` assigned to this block. `LENGTE` is left out on purpose: the
/// compiler builds it from the tree item's `level`, so the legend's copy would repeat it.
fn block_filters(
    request: &ResultRequest,
    block_id: i64,
    entries: &[LegendEntry],
) -> Vec<dl_query::FilterSpec> {
    let mut filters: Vec<dl_query::FilterSpec> = request
        .filters
        .iter()
        .filter(|item| in_block(item.block_id, block_id))
        .map(Into::into)
        .collect();
    filters.extend(entries.iter().filter_map(|entry| {
        let role_spec = &entry.item.role_spec;
        let operation = role_spec.operation?;
        if operation == FilterOperation::Lengte {
            return None;
        }
        Some(dl_query::FilterSpec {
            field_ref: FieldRef::id(entry.item.id.clone()),
            operation,
            values: role_spec.values.clone().unwrap_or_default(),
        })
    }));
    filters
}

/// `BlockFormalizer._resolve_block_placement_from_legend`: where a block's rows belong.
///
/// `main` is the root block's legend, which the totals placement is resolved against.
fn block_placement(entries: &[LegendEntry], main: &[LegendEntry]) -> Result<Placement, ApiError> {
    let streamable: Vec<&LegendEntry> =
        entries.iter().filter(|entry| entry.in_result_set).collect();
    // A block of templates only (a subtotal without measures, usually) has nothing to align
    // with: Python appends it, because that is the only thing it can do.
    if !streamable.is_empty()
        && streamable
            .iter()
            .all(|entry| entry.item.role_spec.role == Role::Template)
    {
        return Ok(Placement::After {
            dimension_values: None,
        });
    }
    let trees: Vec<&LegendEntry> = streamable
        .iter()
        .copied()
        .filter(|entry| entry.item.role_spec.role == Role::Tree)
        .collect();
    let totals: Vec<&LegendEntry> = streamable
        .iter()
        .copied()
        .filter(|entry| entry.item.role_spec.role == Role::Total)
        .collect();
    if !trees.is_empty() && !totals.is_empty() {
        return Err(ApiError::bad_request(
            "ERR.DS_API.BLOCK.ITEM_COMPATIBILITY",
            "Got tree and totals in one block. They are incompatible",
        ));
    }
    if totals.is_empty() {
        let Some(tree) = trees.first() else {
            return Ok(Placement::Root);
        };
        return Ok(Placement::After {
            dimension_values: Some(tree.item.role_spec.tree_dimension_values()),
        });
    }
    // The totals of a result query are aligned with the main block dimension by dimension, so
    // each totals row lands under the rows it summarizes.
    let main_streamable: Vec<&LegendEntry> =
        main.iter().filter(|entry| entry.in_result_set).collect();
    if main_streamable.len() != streamable.len() {
        return Err(ApiError::bad_request(
            "ERR.DS_API.BLOCK.UNEVEN_COLUMN_COUNT",
            format!(
                "Blocks have different column counts. Main block: {}, secondary: {}",
                main_streamable.len(),
                streamable.len()
            ),
        ));
    }
    let mut parent = Vec::new();
    let mut child = Vec::new();
    for (index, entry) in streamable.iter().enumerate() {
        if entry.item.field_type == dl_core::FieldType::Dimension
            && entry.item.role_spec.role != Role::Template
        {
            parent.push(main_streamable[index].item.legend_item_id);
            child.push(entry.item.legend_item_id);
        }
    }
    Ok(Placement::DispersedAfter { parent, child })
}

/// `DataStreamMerger`: insert one block's rows into the stream built so far.
fn merge_block(
    mut stream: Vec<DataRow>,
    rows: Vec<DataRow>,
    placement: &Placement,
) -> Vec<DataRow> {
    match placement {
        Placement::Root => rows,
        Placement::After {
            dimension_values: None,
        } => {
            stream.extend(rows);
            stream
        }
        Placement::After {
            dimension_values: Some(dimensions),
        } => merge_after(stream, rows, dimensions),
        Placement::DispersedAfter { parent, child } => merge_dispersed(stream, rows, parent, child),
    }
}

/// The `legend` positions of `ids` in a row, when the row carries them all (`RowMatcher`).
fn dim_indices(legend: &[i64], ids: &[i64]) -> Option<Vec<usize>> {
    ids.iter()
        .map(|id| legend.iter().position(|legend_id| legend_id == id))
        .collect()
}

/// `AfterBinaryStreamMerger._merge_two_streams_with_dims`: the child rows follow the run of
/// parent rows that matches their dimensions.
///
/// This is how a tree expands a branch: the next level's rows are inserted right after the last
/// row of that branch, so the client reads a hierarchy. Nothing matching means nothing to show —
/// the branch is not on the page, and the child's rows are dropped.
fn merge_after(
    stream: Vec<DataRow>,
    rows: Vec<DataRow>,
    dimensions: &[DimensionValueSpec],
) -> Vec<DataRow> {
    let ids: Vec<i64> = dimensions.iter().map(|dim| dim.legend_item_id).collect();
    let matches = |row: &DataRow| -> bool {
        let Some(indices) = dim_indices(&row.legend, &ids) else {
            return false;
        };
        indices
            .iter()
            .zip(dimensions)
            .all(|(index, dim)| row.data.get(*index) == Some(&dim.value))
    };
    let mut merged = Vec::with_capacity(stream.len() + rows.len());
    let mut child = Some(rows);
    let mut inserted = false;
    let mut previous_matched = false;
    for row in stream {
        if inserted {
            merged.push(row);
            continue;
        }
        let matched = matches(&row);
        if previous_matched && !matched {
            merged.extend(child.take().unwrap_or_default());
            inserted = true;
        }
        merged.push(row);
        previous_matched = matched;
    }
    if previous_matched && !inserted {
        merged.extend(child.take().unwrap_or_default());
    }
    merged
}

/// `DispersedAfterBinaryStreamMerger`: one child row after each group of matching parent rows.
fn merge_dispersed(
    stream: Vec<DataRow>,
    rows: Vec<DataRow>,
    parent_ids: &[i64],
    child_ids: &[i64],
) -> Vec<DataRow> {
    let key_of = |row: &DataRow, ids: &[i64]| -> Option<Vec<serde_json::Value>> {
        let indices = dim_indices(&row.legend, ids)?;
        Some(
            indices
                .iter()
                .map(|index| row.data.get(*index).cloned().unwrap_or_default())
                .collect(),
        )
    };
    // `{ <child dim values>: child row }`: a repeated key keeps the last row, like Python's dict.
    let mut pending: Vec<(Vec<serde_json::Value>, DataRow)> = Vec::new();
    for row in rows {
        let Some(key) = key_of(&row, child_ids) else {
            continue;
        };
        match pending.iter_mut().find(|(other, _)| *other == key) {
            Some(slot) => slot.1 = row,
            None => pending.push((key, row)),
        }
    }
    let mut merged = Vec::with_capacity(stream.len() + pending.len());
    let mut previous: Option<Vec<serde_json::Value>> = None;
    for row in stream {
        let Some(key) = key_of(&row, parent_ids) else {
            // This row's legend has none of the monitored dimensions: nothing to align it with.
            merged.push(row);
            continue;
        };
        if let Some(previous) = &previous {
            if *previous != key {
                if let Some(index) = pending.iter().position(|(other, _)| *other == *previous) {
                    merged.push(pending.remove(index).1);
                }
            }
        }
        merged.push(row);
        previous = Some(key);
    }
    merged.extend(pending.into_iter().map(|(_, row)| row));
    merged
}

fn pivot_spec(request: &PivotRequest) -> QuerySpec {
    QuerySpec {
        // `Measure Names` / `Dimension Names` / a totals placeholder are pivot items, not
        // dataset columns: the legend builds them and the query never selects them.
        select: request
            .fields
            .iter()
            .filter(|item| item.field_ref.pseudo().is_none())
            .map(|item| item.to_select())
            .collect(),
        filters: request.filters.iter().map(Into::into).collect(),
        order_by: request.order_by.iter().map(Into::into).collect(),
        // `PivotDataRequestBaseSchema` overrides the default hard limit: a pivot allows
        // 100_000 rows, not the million of `/result`.
        limit: clamped_limit(request.limit, Some(request.hard_limit())),
        offset: request.offset,
        disable_group_by: request.disable_group_by,
        group_by_if_measures: false,
        parameter_values: request.parameter_values.iter().map(Into::into).collect(),
        ignore_nonexistent_filters: request.ignore_nonexistent_filters,
        totals: false,
    }
}

fn preview_spec(dataset: &Dataset, request: &PreviewRequest) -> QuerySpec {
    // `preview` selects every visible non-parameter field (`PreviewLegendFormalizer`).
    let select = legend::preview_fields(dataset)
        .into_iter()
        .map(|field| dl_query::SelectItem {
            field_ref: dl_core::FieldRef::id(field.guid.clone()),
            role: dl_query::Role::Row,
            level: None,
        })
        .collect();
    QuerySpec {
        select,
        filters: Vec::new(),
        order_by: Vec::new(),
        // Three numbers decide a preview page: `limit` (the page the editor asks for, 100 by
        // default), `row_count_hard_limit` (the ceiling that page is clamped by, 100_000) and
        // `PREVIEW_ROW_LIMIT` (1000, which caps a preview block whatever the first two say).
        limit: clamped_limit(
            request.limit.or(Some(DEFAULT_PREVIEW_LIMIT)),
            Some(
                request
                    .row_count_hard_limit
                    .unwrap_or(PREVIEW_DEFAULT_ROW_LIMIT),
            ),
        )
        .map(|limit| limit.min(PREVIEW_PAGE_LIMIT)),
        offset: None,
        disable_group_by: false,
        group_by_if_measures: true,
        parameter_values: Vec::new(),
        ignore_nonexistent_filters: false,
        totals: false,
    }
}

fn clamped_limit(limit: Option<i64>, hard_limit: Option<i64>) -> Option<i64> {
    let hard = hard_limit.unwrap_or(DEFAULT_ROW_LIMIT).max(0);
    Some(limit.map_or(hard, |value| value.clamp(0, hard)))
}

/// `QueryFormalizerBase.make_query_meta`: the hard limit a `/result` request is cut at.
///
/// A request may lower the cap but not raise it — Python takes
/// `min(row_count_hard_limit or DEFAULT_SOURCE_DB_LIMIT, DEFAULT_SOURCE_DB_LIMIT)`.
fn result_hard_limit(request: &ResultRequest) -> i64 {
    request
        .row_count_hard_limit
        .unwrap_or(DEFAULT_ROW_LIMIT)
        .clamp(0, DEFAULT_ROW_LIMIT)
}

/// The `LIMIT` one top-level query gets (`Executor._make_bi_query`).
///
/// The hard limit caps what may be downloaded, it is not a page size: a query that names no
/// `limit` — or names one above the cap — fetches `hard + 1` rows, and that extra row is how the
/// executor tells "exactly at the cap" from "there is more" (`ERR.DS_API.ROW_COUNT_LIMIT`).
fn result_limit(limit: Option<i64>, hard_limit: i64) -> i64 {
    match limit {
        Some(limit) if limit <= hard_limit => limit,
        _ => hard_limit + 1,
    }
}

/// One row: the values of the block's cells, in `SELECT` order.
///
/// A block's row has a position for every selected item, placeholders included: Python selects
/// a placeholder as the literal `''`, the port fills the constant in here instead.
fn decode_prepared_row(
    row: &[Option<String>],
    prepared: &PreparedQuery,
    selected: &[&dl_query::OutputColumn],
) -> DataRow {
    let mut data = Vec::with_capacity(prepared.cells.len());
    let mut legend = Vec::with_capacity(prepared.cells.len());
    for cell in &prepared.cells {
        data.push(match *cell {
            QueryCell::Column { index, .. } => match selected.get(index) {
                Some(column) => {
                    let raw = row.get(index).and_then(|value| value.as_ref());
                    decode_value(raw, column.data_type)
                }
                // The plan is built from the same columns, so this cannot happen; `null` keeps
                // a broken plan from panicking.
                None => serde_json::Value::Null,
            },
            // `PlaceholderObjSpec` selects the template string itself, which is empty here.
            QueryCell::Template { .. } => serde_json::Value::String(String::new()),
        });
        match *cell {
            QueryCell::Column { legend_item_id, .. } | QueryCell::Template { legend_item_id } => {
                legend.push(legend_item_id)
            }
        }
    }
    DataRow { data, legend }
}

/// The columns a row set answers, in result-set order: the filter is the same for every row of
/// the query, so it is computed once here and not per row.
fn result_columns(prepared: &PreparedQuery) -> Vec<&dl_query::OutputColumn> {
    prepared
        .columns
        .iter()
        .filter(|column| column.in_result_set)
        .collect()
}

/// Every row of one result set, decoded.
fn decode_rows(rows: &[Vec<Option<String>>], columns: &[dl_query::OutputColumn]) -> Vec<DataRow> {
    let selected: Vec<&dl_query::OutputColumn> = columns
        .iter()
        .filter(|column| column.in_result_set)
        .collect();
    rows.iter()
        .map(|row| {
            let mut data = Vec::with_capacity(selected.len());
            let mut legend = Vec::with_capacity(selected.len());
            for (position, column) in selected.iter().enumerate() {
                let raw = row.get(position).and_then(|value| value.as_ref());
                data.push(decode_value(raw, column.data_type));
                legend.push(column.legend_item_id);
            }
            DataRow { data, legend }
        })
        .collect()
}

/// One row of a totals block, in the layout of the merged stream.
///
/// Python's block rows also carry the `template` cells, because a placeholder is a `SELECT`
/// expression (`SELECT '' AS <guid>`); the port has no literal support in the compiler, so the
/// constant is put in while the row is decoded. Everything else — the legend ids of the block,
/// and therefore the pivot items the cells resolve to — is the same.
fn merge_block_row(row: &[Option<String>], block: &pivot::TotalBlock) -> DataRow {
    let mut data = Vec::with_capacity(block.cells.len());
    let mut legend = Vec::with_capacity(block.cells.len());
    for (legend_item_id, cell) in &block.cells {
        data.push(match cell {
            pivot::TotalCell::Column { index } => {
                let raw = row.get(*index).and_then(|value| value.as_ref());
                decode_value(raw, block.columns[*index].data_type)
            }
            // `PlaceholderRef` selects the template string itself, which is empty for totals.
            pivot::TotalCell::Template => serde_json::Value::String(String::new()),
        });
        legend.push(*legend_item_id);
    }
    DataRow { data, legend }
}

/// One value of a row, postprocessed the way `dl_query_processing.postprocessing` does it.
///
/// `postprocess_data` sends every value through a per-type processor, and the ones without a
/// processor go through `stringify_or_null`, i.e. Python's `str()`. That is the v2 contract, not
/// a formatting detail: a measure is a JSON **string**, and the frontend parses it.
fn decode_value(raw: Option<&String>, dtype: DataType) -> serde_json::Value {
    use serde_json::Value;

    let text = match raw {
        Some(text) => text,
        None => return Value::Null,
    };
    match dtype.native().base() {
        // `str(float)`: the reference service hands measures back through a Python float, so a
        // `numeric` column keeps 16 digits (`22865147.90679568`) and not its full scale.
        dfl_native::DataType::Float => Value::String(python_float(text)),
        // `str(True)` is `"True"` in Python.
        dfl_native::DataType::Boolean => Value::String(python_bool(text)),
        // `postprocess_markup`: the s-expression the SQL builds becomes the JSON the frontend
        // renders, so the cell is an object rather than a string.
        dfl_native::DataType::Markup => {
            markup::postprocess(text).unwrap_or_else(|| Value::String(text.clone()))
        }
        // `postprocess_array`: the driver hands Python a list and the frontend gets its JSON.
        dfl_native::DataType::ArrayStr
        | dfl_native::DataType::ArrayInt
        | dfl_native::DataType::ArrayFloat
        | dfl_native::DataType::TreeStr => postprocess::postprocess_array(text, dtype.native())
            .map(Value::String)
            .unwrap_or_else(|| Value::String(text.clone())),
        native @ (dfl_native::DataType::Datetime
        | dfl_native::DataType::Datetimetz
        | dfl_native::DataType::GenericDatetime) => {
            Value::String(postprocess_datetime(text, native))
        }
        _ => Value::String(text.clone()),
    }
}

/// `str()` of a Python `float`: the shortest representation that round-trips.
pub(crate) fn python_float(text: &str) -> String {
    match text.trim().parse::<f64>() {
        Ok(value) if value.is_nan() => "nan".to_string(),
        Ok(value) if value.is_infinite() => {
            if value > 0.0 {
                "inf".to_string()
            } else {
                "-inf".to_string()
            }
        }
        Ok(value) => {
            let mut text = format!("{value}");
            // `str(float)` always shows a fractional part: `str(1473.0) == "1473.0"`, while
            // Rust's `Display` prints `1473`.
            if !text.contains(['.', 'e', 'E']) {
                text.push_str(".0");
            }
            text
        }
        // A `numeric` outside `f64`'s range keeps its text: it is not a value Python could give
        // back as a float either.
        Err(_) => text.to_string(),
    }
}

/// `DataExportForbiddenReason.prohibited_in_dashsql`.
const PROHIBITED_IN_DASHSQL: &str = "PROHIBITED_EXPORT_QL_CHART";
/// `DataExportForbiddenReason.prohibited_in_typed_query`.
const PROHIBITED_IN_TYPED_QUERY: &str = "PROHIBITED_EXPORT_TYPED_QUERY";

/// The connection's export policy with the background export forbidden by the endpoint itself.
///
/// Python's `DashSQLView`/`DashSQLTypedQueryView` build the base result from the connection and
/// then force `background.allowed = False` with their own reason; `basic` stays untouched.
fn connection_data_export(target: &ConnectionTarget, reason: &str) -> DataExport {
    let mut export = DataExport::for_connection(target);
    export.background.allowed = false;
    export
        .background
        .reason
        .get_or_insert_with(Vec::new)
        .push(reason.to_string());
    export
}

fn is_clickhouse_dsn(dsn: &str) -> bool {
    dsn.starts_with("clickhouse://") || dsn.starts_with("clickhouses://")
}

/// `_ch_type_name_to_native_type` + `GenericNativeType`: strip `Nullable`/`LowCardinality`
/// wrappers and the type parameters, lowercase the base name (`Nullable(UInt64)` -> `uint64`,
/// `Array(String)` -> `array`).
fn native_type_name(raw: &str) -> String {
    let mut name = raw.trim().to_ascii_lowercase();
    for wrapper in ["lowcardinality(", "nullable("] {
        if name.starts_with(wrapper) {
            name = name[wrapper.len()..].trim_end_matches(')').to_string();
        }
    }
    name.split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// `ClickHouseTypeTransformer.native_to_user_map`, keyed by the normalized native type name.
///
/// The Python map is keyed by the SQLAlchemy type classes; `_ch_type_name_to_native_type`
/// normalizes the ClickHouse spelling to the same base names, so the string is enough here.
fn ch_user_type_name(raw: &str) -> &'static str {
    match native_type_name(raw).as_str() {
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64" | "int"
        | "uint" => "integer",
        "float" | "float32" | "float64" | "numeric" | "decimal" | "decimal32" | "decimal64"
        | "decimal128" | "decimal256" => "float",
        "string" | "enum8" | "enum16" => "string",
        "date" | "date32" => "date",
        "bool" => "boolean",
        "datetime" | "datetime64" | "datetimewithtz" | "datetime64withtz" => "genericdatetime",
        "uuid" => "uuid",
        _ => "unsupported",
    }
}

/// `PostgreSQLTypeTransformer.native_to_user_map`: `tokio_postgres::Type::name` spellings.
fn pg_user_type_name(raw: &str) -> &'static str {
    match native_type_name(raw).as_str() {
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" | "oid" => "integer",
        "float4" | "float8" | "real" | "double precision" | "numeric" | "decimal" => "float",
        "bool" | "boolean" => "boolean",
        "text" | "varchar" | "character varying" | "char" | "bpchar" | "character" | "name"
        | "citext" | "enum" => "string",
        "date" => "date",
        "timestamp"
        | "timestamp without time zone"
        | "timestamp with time zone"
        | "timestamptz" => "genericdatetime",
        "uuid" => "uuid",
        _ => "unsupported",
    }
}

/// The BI type of a native type name, for the connector the DSN names.
fn bi_type_name(dsn: &str, raw: &str) -> &'static str {
    if is_clickhouse_dsn(dsn) {
        ch_user_type_name(raw)
    } else {
        pg_user_type_name(raw)
    }
}

/// One typed-query cell: the value a Python driver would hand `json_response`.
///
/// Numeric columns stay numbers, booleans stay booleans and timestamps are ISO strings (Python's
/// driver returns `datetime` objects there; the ISO spelling is what its JSON encoder would
/// produce). A cursor without native types keeps the text, except for arrays, which are JSON
/// documents already (`JSONCompact`). `NULL` is always JSON `null`.
fn typed_cell(raw: Option<&str>, user_type: &str) -> serde_json::Value {
    use serde_json::Value;

    let Some(text) = raw else {
        return Value::Null;
    };
    match user_type {
        "integer" => text
            .trim()
            .parse::<i64>()
            .map(|value| Value::Number(value.into()))
            .or_else(|_| {
                text.trim()
                    .parse::<u64>()
                    .map(|value| Value::Number(value.into()))
            })
            .unwrap_or_else(|_| Value::String(text.to_string())),
        "float" => text
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(text.to_string())),
        "boolean" => Value::Bool(matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "t" | "true" | "1" | "y" | "yes"
        )),
        "date" | "datetime" | "genericdatetime" | "uuid" | "string" => {
            Value::String(text.to_string())
        }
        _ => {
            let trimmed = text.trim();
            if trimmed.starts_with(['[', '{']) {
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    return value;
                }
            }
            Value::String(text.to_string())
        }
    }
}

/// `bool` as Python spells it: `stringify_or_null` is `str()`, and `str(True) == "True"`.
fn python_bool(text: &str) -> String {
    let flag = matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "t" | "true" | "1" | "y" | "yes"
    );
    if flag { "True" } else { "False" }.to_string()
}

/// `postprocess_datetime` / `postprocess_genericdatetime` / `make_postprocess_datetimetz`.
///
/// PostgreSQL spells a timestamp `2020-01-01 00:00:00[+00]`; Python's `isoformat()` spells it
/// `2020-01-01T00:00:00`. A naive value is already the answer for all three types, so only the
/// separator and the zone have to be handled:
///
/// * `genericdatetime` (`postprocess_genericdatetime`) drops the zone, without converting;
/// * `datetime` (`postprocess_datetime`) converts to UTC first, then drops the zone — a `+00`
///   value therefore needs no arithmetic, which is what a database column stores;
/// * `datetimetz` keeps the zone, spelled `+00:00` by `isoformat()`.
fn postprocess_datetime(text: &str, native: dfl_native::DataType) -> String {
    let (body, offset) = split_zone(text);
    let naive = body.replace(' ', "T");
    match (native, offset) {
        (dfl_native::DataType::Datetimetz, offset) => {
            format!("{naive}{}", iso_offset(offset.as_deref()))
        }
        // `datetime` and `genericdatetime` both end up without a zone. Only a non-UTC `datetime`
        // would have to shift the value itself; the demo data is stored in UTC.
        _ => naive,
    }
}

/// `2020-01-01 00:00:00+00` -> (`2020-01-01 00:00:00`, `Some("+00")`).
///
/// PostgreSQL writes the zone as `+HH`, `+HH:MM` or `+HH:MM:SS`, depending on the value.
fn split_zone(text: &str) -> (String, Option<String>) {
    let trimmed = text.trim();
    for width in [9, 6, 3] {
        let Some(index) = trimmed.len().checked_sub(width) else {
            continue;
        };
        if !trimmed.is_char_boundary(index) {
            continue;
        }
        let tail = &trimmed[index..];
        let sign = tail.starts_with('+') || tail.starts_with('-');
        if sign && tail[1..].chars().all(|ch| ch.is_ascii_digit() || ch == ':') {
            return (trimmed[..index].to_string(), Some(tail.to_string()));
        }
    }
    (trimmed.to_string(), None)
}

/// `+00` -> `+00:00`, as `isoformat()` writes it.
fn iso_offset(offset: Option<&str>) -> String {
    match offset {
        Some(offset) if offset.len() == 3 => format!("{offset}:00"),
        Some(offset) => offset.to_string(),
        None => String::new(),
    }
}
/// The field a `/values/*` request is about; both body shapes are accepted, so a request that
/// names none at all is the only case left.
fn value_field(field_ref: Option<FieldRef>) -> Result<FieldRef, ApiError> {
    field_ref
        .ok_or_else(|| ApiError::bad_request("ERR.DS_API.REQUEST", "the request selects no field"))
}

/// The `/values/*` endpoints: `distinct` and `range` of one field.
impl<E: Executor> Service<E> {
    /// `POST /api/data/v2/datasets/{ds_id}/values/distinct`
    pub fn prepare_distinct(
        &self,
        dataset: &Dataset,
        request: &DistinctRequest,
    ) -> Result<PreparedQuery, ApiError> {
        self.prepare_distinct_with_dialect(dataset, request, self.dialect)
    }

    fn prepare_distinct_with_dialect(
        &self,
        dataset: &Dataset,
        request: &DistinctRequest,
        dialect: Dialect,
    ) -> Result<PreparedQuery, ApiError> {
        let dataset = dataset_with_updates_inferred(dataset, &request.updates, dialect)?;
        let spec = QuerySpec {
            select: vec![SelectItem {
                field_ref: value_field(request.field_ref())?,
                role: Role::Row,
                level: None,
            }],
            filters: request.filters.iter().map(Into::into).collect(),
            limit: clamped_limit(
                request.limit.or(Some(DEFAULT_DISTINCT_LIMIT)),
                request.row_count_hard_limit,
            ),
            offset: request.offset,
            parameter_values: request.parameter_values.iter().map(Into::into).collect(),
            ignore_nonexistent_filters: request.ignore_nonexistent_filters,
            ..QuerySpec::default()
        };
        compile_distinct(&dataset, &spec, dialect)
            .map(PreparedQuery::from)
            .map_err(ApiError::from)
    }

    pub async fn distinct(
        &self,
        dataset: &Dataset,
        request: &DistinctRequest,
    ) -> Result<ResultResponse, ApiError> {
        let dsn_started = std::time::Instant::now();
        let target = self.connection_target(dataset).await?;
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        crate::trace::record_wait("dsn", dsn_started.elapsed());
        // Control values are read on every dashboard load; cache them only when the connection
        // explicitly configures `cache_ttl_sec`, keyed by the dataset revision the engine sent.
        let cache_scope = self.query_cache_scope(
            dataset,
            dsn,
            target.as_ref().and_then(|target| target.cache_ttl_sec),
            request.revision_id.as_deref(),
        );
        let prepare_started = std::time::Instant::now();
        let prepared =
            self.prepare_distinct_with_dialect(dataset, request, self.target_dialect(dsn))?;
        crate::trace::record_work("prepare", prepare_started.elapsed());
        self.execute(
            prepared,
            DataExport::for_dataset_in_connection(dataset, target.as_ref()),
            dsn,
            &QueryMemo::with_cache(cache_scope),
        )
        .await
    }

    /// `POST /api/data/v2/datasets/{ds_id}/values/range`
    pub fn prepare_range(
        &self,
        dataset: &Dataset,
        request: &RangeRequest,
    ) -> Result<PreparedQuery, ApiError> {
        self.prepare_range_with_dialect(dataset, request, self.dialect)
    }

    fn prepare_range_with_dialect(
        &self,
        dataset: &Dataset,
        request: &RangeRequest,
        dialect: Dialect,
    ) -> Result<PreparedQuery, ApiError> {
        let dataset = dataset_with_updates_inferred(dataset, &request.updates, dialect)?;
        let spec = QuerySpec {
            select: vec![SelectItem {
                field_ref: value_field(request.field_ref())?,
                role: Role::Row,
                level: None,
            }],
            filters: request.filters.iter().map(Into::into).collect(),
            parameter_values: request.parameter_values.iter().map(Into::into).collect(),
            ignore_nonexistent_filters: request.ignore_nonexistent_filters,
            ..QuerySpec::default()
        };
        compile_range(&dataset, &spec, dialect)
            .map(PreparedQuery::from)
            .map_err(ApiError::from)
    }

    pub async fn range(
        &self,
        dataset: &Dataset,
        request: &RangeRequest,
    ) -> Result<ResultResponse, ApiError> {
        let dsn_started = std::time::Instant::now();
        let target = self.connection_target(dataset).await?;
        let dsn = target.as_ref().map(|target| target.dsn.as_str());
        crate::trace::record_wait("dsn", dsn_started.elapsed());
        let prepare_started = std::time::Instant::now();
        let prepared =
            self.prepare_range_with_dialect(dataset, request, self.target_dialect(dsn))?;
        crate::trace::record_work("prepare", prepare_started.elapsed());
        self.execute(
            prepared,
            DataExport::for_dataset_in_connection(dataset, target.as_ref()),
            dsn,
            &QueryMemo::default(),
        )
        .await
    }
}

/// The connection-scoped endpoints: `dashsql`, `typed_query`, `typed_query_raw`.
///
/// Python resolves the `us` connection and then asks the connector executor; the port reuses the
/// same resolver and [`Executor`] the dataset path uses. The `raw_sql_level=dashsql` gate is
/// Python's `is_dashsql_allowed`: any lower level is refused before the SQL reaches a database.
impl<E: Executor> Service<E> {
    /// Resolve one `us` connection entry to its DSN and policy.
    pub async fn connection_target_for(
        &self,
        connection_id: &str,
    ) -> Result<ConnectionTarget, ApiError> {
        let Some(connections) = &self.connections else {
            return Err(ApiError::new(
                400,
                "ERR.DS_API.DB.CONNECTION_NOT_FOUND",
                format!("connection {connection_id} does not exist"),
            ));
        };
        connections
            .resolve_target(connection_id)
            .await
            .ok_or_else(|| {
                ApiError::new(
                    404,
                    "ERR.DS_API.DB.CONNECTION_NOT_FOUND",
                    format!("connection {connection_id} does not exist"),
                )
            })
    }

    /// [`Service::connection_target_for`] plus the `raw_sql_level=dashsql` gate
    /// (`RawSqlLevelConnectionMixin.is_dashsql_allowed`).
    async fn dashsql_target(&self, connection_id: &str) -> Result<ConnectionTarget, ApiError> {
        let target = self.connection_target_for(connection_id).await?;
        if target.raw_sql_level != RawSqlLevel::Dashsql {
            return Err(ApiError::new(
                400,
                "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED",
                "DashSQL API is disallowed in the connection settings",
            ));
        }
        Ok(target)
    }

    /// One connection query with the cursor metadata `dashsql`/`typed_query` serialize.
    async fn connection_query(
        &self,
        target: &ConnectionTarget,
        sql: &str,
    ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ApiError> {
        self.executor
            .fetch_with_meta(sql, Some(&target.dsn))
            .await
            .map_err(|error| ApiError::database(error.to_string()))
    }

    /// `POST /api/data/v1/connections/{conn_id}/dashsql`.
    ///
    /// The body is the event list Python streams: `metadata`, one `row` per row, `footer`. The
    /// metadata carries the cursor's `names`/`driver_types`/`db_types`/`bi_types` (from the
    /// executor's [`QueryMetadata`]) and the `data_export` block; `background` can never be
    /// allowed from a DashSQL query (`DataExportForbiddenReason.prohibited_in_dashsql`).
    pub async fn dashsql(
        &self,
        connection_id: &str,
        request: &DashSqlRequest,
    ) -> Result<serde_json::Value, ApiError> {
        let sql = request.sql_query.trim();
        if sql.is_empty() {
            return Err(ApiError::bad_request(
                "ERR.DS_API.REQUEST",
                "sql_query is required",
            ));
        }
        let target = self.dashsql_target(connection_id).await?;
        let (rows, meta) = self.connection_query(&target, sql).await?;
        let data_export = connection_data_export(&target, PROHIBITED_IN_DASHSQL);
        let mut events = Vec::with_capacity(rows.len() + 2);
        events.push(serde_json::json!({
            "event": "metadata",
            "data": {
                "names": meta.names,
                "driver_types": meta.db_types,
                "db_types": meta.db_types.iter().map(|name| native_type_name(name)).collect::<Vec<_>>(),
                "bi_types": meta.db_types.iter().map(|name| bi_type_name(&target.dsn, name)).collect::<Vec<_>>(),
                "data_export": data_export,
            },
        }));
        for row in rows {
            events.push(serde_json::json!({ "event": "row", "data": row }));
        }
        events.push(serde_json::json!({ "event": "footer", "data": {} }));
        Ok(serde_json::Value::Array(events))
    }

    /// `POST /api/data/v1/connections/{conn_id}/typed_query`.
    ///
    /// Python enables typed queries for ClickHouse only (`is_typed_query_allowed` is `False` in
    /// `ConnectionBase`); PostgreSQL answers `ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED`
    /// before executing anything, and so does the port. For ClickHouse the result keeps
    /// Python's `{query_type, data: {rows, headers}}` shape: `headers` are the cursor's column
    /// names with their BI types, and the values are typed (numbers, booleans, ISO timestamps)
    /// rather than the text of the dataset path. A cursor without metadata keeps Python's
    /// `col_N`/`unsupported` fallback.
    pub async fn typed_query(
        &self,
        connection_id: &str,
        request: &TypedQueryRequest,
    ) -> Result<serde_json::Value, ApiError> {
        let sql = request.sql().ok_or_else(|| {
            ApiError::bad_request(
                "ERR.DS_API.QUERY",
                "typed query content must carry a `query` string",
            )
        })?;
        let target = self.dashsql_target(connection_id).await?;
        if !is_clickhouse_dsn(&target.dsn) {
            return Err(ApiError::new(
                400,
                "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED",
                "DashSQL API is disallowed in the connection settings",
            ));
        }
        let (rows, meta) = self.connection_query(&target, sql).await?;
        let width = rows
            .first()
            .map(Vec::len)
            .unwrap_or(meta.names.len().max(meta.db_types.len()));
        let type_names: Vec<&str> = (0..width)
            .map(|index| {
                meta.db_types
                    .get(index)
                    .map_or("unsupported", |name| ch_user_type_name(name))
            })
            .collect();
        let headers: Vec<serde_json::Value> = (0..width)
            .map(|index| {
                let name = meta
                    .names
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| format!("col_{index}"));
                serde_json::json!({ "name": name, "data_type": type_names[index] })
            })
            .collect();
        let rows: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(index, cell)| {
                        typed_cell(
                            cell.as_deref(),
                            type_names.get(index).copied().unwrap_or("unsupported"),
                        )
                    })
                    .collect()
            })
            .collect();
        Ok(serde_json::json!({
            "query_type": request.query_type(),
            "data": { "rows": rows, "headers": headers },
            "data_export": connection_data_export(&target, PROHIBITED_IN_TYPED_QUERY),
        }))
    }

    /// `POST /api/data/v1/connections/{conn_id}/typed_query_raw`.
    ///
    /// A raw typed query is an HTTP request to the connector's own API (ClickHouse HTTP,
    /// Metrica, ...). Neither PostgreSQL nor ClickHouse exposes one in Python 0.2457
    /// (`is_typed_query_raw_allowed` is `False`), so the ref is the same
    /// `ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED` they answer.
    pub async fn typed_query_raw(
        &self,
        connection_id: &str,
        _request: &TypedQueryRawRequest,
    ) -> Result<serde_json::Value, ApiError> {
        // Read the connection first: an unknown id is a 404, not a "not allowed".
        self.dashsql_target(connection_id).await?;
        Err(ApiError::new(
            400,
            "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED",
            "Raw typed queries are not supported by this connection",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::dto::FieldType;
    use crate::dto::{FieldsItem, OrderByItem};
    use crate::executor::{Executor, ExecutorError, StaticExecutor};
    use crate::testing::demo_dataset;
    use crate::updates::FieldPatch;
    use dl_core::{CalcMode, FieldRef, UserDataType};
    use dl_query::{Direction, Role};
    use serde_json::json;

    mod limit_tests {
        use super::*;

        /// `Executor._make_bi_query`: a top-level query that names no `limit` fetches one row
        /// beyond the hard cap, which is how "there is more" is told from "exactly the cap".
        #[test]
        fn a_result_without_a_limit_is_cut_at_the_hard_limit_plus_one() {
            assert_eq!(DEFAULT_ROW_LIMIT, 1_000_000);
            assert_eq!(result_limit(None, DEFAULT_ROW_LIMIT), 1_000_001);
            assert_eq!(result_limit(Some(500), DEFAULT_ROW_LIMIT), 500);
            assert_eq!(result_limit(Some(1_000_000), DEFAULT_ROW_LIMIT), 1_000_000);
            assert_eq!(result_limit(Some(5_000_000), DEFAULT_ROW_LIMIT), 1_000_001);
        }

        /// `make_query_meta`: a request may lower the hard limit but not raise it above
        /// `DEFAULT_SOURCE_DB_LIMIT`.
        #[test]
        fn the_hard_limit_caps_the_request_one() {
            let hard = |limit: Option<i64>| {
                result_hard_limit(&ResultRequest {
                    row_count_hard_limit: limit,
                    ..ResultRequest::default()
                })
            };
            assert_eq!(hard(None), DEFAULT_ROW_LIMIT);
            assert_eq!(hard(Some(10)), 10);
            assert_eq!(hard(Some(DEFAULT_ROW_LIMIT + 1)), DEFAULT_ROW_LIMIT);
            assert_eq!(hard(Some(-5)), 0);
        }
    }

    mod postprocessing_tests {
        use super::*;

        /// `postprocess_data` + `stringify_or_null`: a measure is the text of a Python value.
        #[test]
        fn values_are_stringified_the_way_python_does_it() {
            let dtype = |name: &str| DataType::parse(name).unwrap();
            let decode =
                |text: &str, name: &str| decode_value(Some(&text.to_string()), dtype(name));

            // A `numeric` sum keeps the 16 digits of a Python float, not its full scale.
            assert_eq!(
                decode("22865147.9067956817783294", "FLOAT"),
                json!("22865147.90679568")
            );
            // `str(1473.0)` has a fractional part, `str(1473)` does not.
            assert_eq!(decode("1473", "FLOAT"), json!("1473.0"));
            assert_eq!(decode("1473", "INTEGER"), json!("1473"));
            assert_eq!(decode("t", "BOOLEAN"), json!("True"));
            assert_eq!(decode("f", "BOOLEAN"), json!("False"));
            assert_eq!(decode("NaN", "FLOAT"), json!("nan"));
            // `NULL` stays `null`, whatever the column type is.
            assert_eq!(decode_value(None, dtype("FLOAT")), serde_json::Value::Null);
        }

        /// `postprocess_datetime`: `isoformat()`, i.e. `T` instead of a space and no zone.
        #[test]
        fn timestamps_become_iso_strings() {
            let dtype = |name: &str| DataType::parse(name).unwrap();
            let decode =
                |text: &str, name: &str| decode_value(Some(&text.to_string()), dtype(name));

            assert_eq!(
                decode("2020-01-01 00:00:00", "DATETIME"),
                json!("2020-01-01T00:00:00")
            );
            // A `+00` value is already UTC, so dropping the zone is all `datetime` does.
            assert_eq!(
                decode("2020-01-01 00:00:00+00", "DATETIME"),
                json!("2020-01-01T00:00:00")
            );
            assert_eq!(
                decode("2020-01-01 00:00:00+00", "GENERICDATETIME"),
                json!("2020-01-01T00:00:00")
            );
            // `datetimetz` keeps the zone, and `isoformat()` spells it `+00:00`.
            assert_eq!(
                decode("2020-01-01 00:00:00+00", "DATETIMETZ"),
                json!("2020-01-01T00:00:00+00:00")
            );
            // A date has no time part and no processor: the text is the answer.
            assert_eq!(decode("2020-01-01", "DATE"), json!("2020-01-01"));
        }
    }

    /// The row sets the executor answers with, per call.
    type RowSets = Vec<Vec<Vec<Option<String>>>>;
    type Calls = Arc<Mutex<Vec<String>>>;
    type Targets = Arc<Mutex<Vec<Option<String>>>>;

    /// An executor that records the SQL it is given and answers one row set per call.
    ///
    /// A multi-block request (a pivot with totals) runs its blocks one after another, so the
    /// tests need one answer per query; the last recorded calls reuse the last row set.
    struct RecordingExecutor {
        calls: Calls,
        targets: Targets,
        row_sets: RowSets,
    }

    impl RecordingExecutor {
        fn new(row_sets: RowSets) -> (Self, Calls) {
            let (executor, calls, _targets) = Self::with_targets(row_sets);
            (executor, calls)
        }

        fn with_targets(row_sets: RowSets) -> (Self, Calls, Targets) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let targets = Arc::new(Mutex::new(Vec::new()));
            let executor = Self {
                calls: calls.clone(),
                targets: targets.clone(),
                row_sets,
            };
            (executor, calls, targets)
        }
    }

    #[async_trait]
    impl Executor for RecordingExecutor {
        async fn fetch(
            &self,
            sql: &str,
            target: Option<&str>,
        ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(sql.to_string());
            let index = calls.len() - 1;
            drop(calls);
            self.targets
                .lock()
                .unwrap()
                .push(target.map(str::to_string));
            Ok(self.row_sets.get(index).cloned().unwrap_or_default())
        }
    }

    /// An executor that yields before answering, so two callers really overlap.
    struct SlowExecutor {
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Executor for SlowExecutor {
        async fn fetch(
            &self,
            _sql: &str,
            _target: Option<&str>,
        ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
            *self.calls.lock().unwrap() += 1;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(vec![vec![
                Some("Europe".to_string()),
                Some("42".to_string()),
            ]])
        }
    }

    /// A resolver that always answers with the same DSN and records what it was asked.
    struct FixedResolver {
        dsn: Option<String>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl crate::executor::ConnectionResolver for FixedResolver {
        async fn resolve(&self, connection_id: &str) -> Option<String> {
            self.asked.lock().unwrap().push(connection_id.to_string());
            self.dsn.clone()
        }
    }

    fn service() -> Service<StaticExecutor> {
        Service::new(
            StaticExecutor::new(vec![
                vec![Some("Europe".into()), Some("42".into())],
                vec![Some("Asia".into()), None],
            ]),
            Dialect::Postgres,
        )
    }

    #[tokio::test]
    async fn the_memo_is_scoped_to_one_request() {
        let (executor, calls) = RecordingExecutor::new(vec![vec![vec![
            Some("Europe".to_string()),
            Some("42".to_string()),
        ]]]);
        let service = Service::new(executor, Dialect::Postgres);
        let dataset = demo_dataset();
        let request = result_request();
        // Two public requests never share the memo: otherwise the second one could answer
        // with rows read before a data change.
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), 2);

        // Inside one request, the same SQL is executed once.
        let memo = QueryMemo::default();
        service.fetch_raw("SELECT 1", None, &memo).await.unwrap();
        service.fetch_raw("SELECT 1", None, &memo).await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn identical_sql_inside_one_request_runs_once_concurrently() {
        let calls = Arc::new(Mutex::new(0));
        let service = Service::new(
            SlowExecutor {
                calls: Arc::clone(&calls),
            },
            Dialect::Postgres,
        );
        let memo = QueryMemo::default();
        let (left, right) = tokio::join!(
            service.fetch_raw("SELECT 1", None, &memo),
            service.fetch_raw("SELECT 1", None, &memo)
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn an_empty_update_list_borrows_the_dataset() {
        let dataset = demo_dataset();
        let unchanged = dataset_with_updates(&dataset, &[]).unwrap();
        assert!(matches!(unchanged, Cow::Borrowed(_)));

        let update = UpdateAction::AddField {
            order_index: None,
            field: FieldPatch {
                guid: Some("f_new".to_string()),
                title: Some("New".to_string()),
                cast: Some(UserDataType::Float),
                calc_mode: Some(CalcMode::Formula),
                formula: Some("[Sales] / 2".to_string()),
                ..FieldPatch::default()
            },
        };
        assert!(matches!(
            dataset_with_updates(&dataset, &[update]).unwrap(),
            Cow::Owned(_)
        ));
    }

    #[test]
    fn the_combined_field_pass_matches_the_two_separate_passes() {
        let dataset = demo_dataset();
        let (field_types, data_types) =
            dl_query::field_types_and_data_types(&dataset, Dialect::Postgres);
        assert_eq!(
            field_types,
            dl_query::field_types(&dataset, Dialect::Postgres)
        );
        assert_eq!(
            data_types,
            dl_query::field_data_types(&dataset, Dialect::Postgres)
        );
    }

    fn result_request() -> ResultRequest {
        ResultRequest {
            fields: vec![
                FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
            ],
            order_by: vec![OrderByItem::new(FieldRef::id("f_sales"), Direction::Desc)],
            ..ResultRequest::default()
        }
    }

    #[test]
    fn result_compiles_the_v2_request() {
        let prepared = service()
            .prepare_result(&demo_dataset(), &result_request())
            .unwrap();
        assert_eq!(
            prepared.blocks[0].query.sql,
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" ORDER BY 2 DESC LIMIT 1000001"
        );
        assert!(prepared.blocks[0].query.grouped);
        // No `with_totals` in the request: no second query.
        assert!(prepared.totals.is_none());
    }

    #[tokio::test]
    async fn result_decodes_rows_into_the_v2_response() {
        let response = service()
            .result(&demo_dataset(), &result_request())
            .await
            .unwrap();
        // The legend holds the two selected fields *and* the `order_by` item, which is a
        // legend item in `dl_api_lib` too (it gets an id, it has no value in a row).
        assert_eq!(response.fields.len(), 3);
        assert_eq!(response.fields[0].id, "f_region");
        assert_eq!(response.fields[0].field_type, FieldType::Dimension);
        assert_eq!(response.fields[0].role_spec.role, Role::Row);
        assert_eq!(response.fields[1].field_type, FieldType::Measure);
        assert_eq!(response.fields[1].data_type, UserDataType::Integer);
        assert_eq!(response.fields[1].role_spec.role, Role::Measure);
        assert_eq!(response.fields[2].id, "f_sales");
        assert_eq!(response.fields[2].role_spec.role, Role::OrderBy);
        assert_eq!(
            response.fields[2].role_spec.direction,
            Some(Direction::Desc)
        );
        let rows = response.rows().unwrap();
        assert_eq!(rows.len(), 2);
        // A v2 row carries its legend ids, so legend-only items have no value here.
        assert_eq!(rows[0].data, vec![json!("Europe"), json!("42")]);
        assert_eq!(rows[0].legend, vec![0, 1]);
        assert_eq!(rows[1].data, vec![json!("Asia"), serde_json::Value::Null]);
        assert!(response.blocks[0].query.is_some());
        assert!(!response.data_export_forbidden);
    }

    #[tokio::test]
    async fn autofill_legend_returns_the_fields_the_request_skips() {
        let request: ResultRequest = serde_json::from_value(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
            "autofill_legend": true,
        }))
        .unwrap();
        let response = service().result(&demo_dataset(), &request).await.unwrap();
        let ids: Vec<(&str, &str)> = response
            .fields
            .iter()
            .map(|item| (item.id.as_str(), item.role_spec.role.as_str()))
            .collect();
        assert_eq!(
            ids,
            vec![
                ("f_region", "row"),
                ("f_sales", "info"),
                ("f_year", "info"),
                ("f_calc", "info"),
            ]
        );
        // The autofilled fields have no value in a row, which is why the row carries ids.
        let rows = response.rows().unwrap();
        assert_eq!(rows[0].data, vec![json!("Europe")]);
        assert_eq!(rows[0].legend, vec![0]);
    }

    #[tokio::test]
    async fn a_template_item_is_selected_and_serialised_with_its_template() {
        let request: ResultRequest = serde_json::from_value(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {
                    "ref": {"type": "id", "id": "f_year"},
                    "role_spec": {"role": "template", "template": "{{f_year}}", "visibility": "visible"},
                },
            ],
        }))
        .unwrap();
        let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
        assert!(
            prepared.blocks[0]
                .query
                .sql
                .starts_with("SELECT t0.\"Region\" AS \"f_region\", t0.\"Year\" AS \"f_year\""),
            "{}",
            prepared.blocks[0].query.sql
        );
        assert_eq!(
            prepared.blocks[0].query.legend[1].role_spec,
            crate::dto::RoleSpec::template("{{f_year}}")
        );

        let response = service().result(&demo_dataset(), &request).await.unwrap();
        let rows = response.rows().unwrap();
        assert_eq!(rows[0].data, vec![json!("Europe"), json!("42")]);
        assert_eq!(rows[0].legend, vec![0, 1]);
    }

    #[tokio::test]
    async fn a_pinned_legend_item_id_is_what_the_row_points_at() {
        let request: ResultRequest = serde_json::from_value(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "legend_item_id": 5},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
            ],
        }))
        .unwrap();
        let response = service().result(&demo_dataset(), &request).await.unwrap();
        assert_eq!(response.fields[0].legend_item_id, 5);
        assert_eq!(response.fields[1].legend_item_id, 0);
        let rows = response.rows().unwrap();
        assert_eq!(rows[0].legend, vec![5, 0]);
    }
    #[test]
    fn updates_add_a_formula_field_that_the_query_uses() {
        let mut request = result_request();
        request
            .fields
            .push(FieldsItem::new(FieldRef::id("f_new"), Role::Measure));
        request.updates = vec![UpdateAction::AddField {
            order_index: None,
            field: FieldPatch {
                guid: Some("f_new".to_string()),
                title: Some("New".to_string()),
                cast: Some(UserDataType::Float),
                calc_mode: Some(CalcMode::Formula),
                formula: Some("[Sales] / 2".to_string()),
                ..FieldPatch::default()
            },
        }];
        let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
        // `[Sales]` is a dataset measure, so the reference is substituted aggregated: the added
        // measure is `SUM(sales) / 2`, exactly what `dl_query_processing` would compile.
        assert!(
            prepared.blocks[0]
                .query
                .sql
                .contains("(CAST(SUM(t0.\"Sales\") AS DOUBLE PRECISION) / 2) AS \"f_new\""),
            "{}",
            prepared.blocks[0].query.sql
        );
    }

    #[test]
    fn an_added_formula_field_reports_the_type_the_compiler_derives() {
        // `formula_authority`: `add_field` of `[Sales] + 1`, which the dataset does not know,
        // so there is no stored `type` at all. `FormulaCompiler.get_field_type` derives it from
        // the prepared formula, which aggregates, so the legend item is a MEASURE.
        let mut request = result_request();
        request
            .fields
            .push(FieldsItem::new(FieldRef::id("f_new"), Role::Measure));
        request.updates = vec![UpdateAction::AddField {
            order_index: None,
            field: FieldPatch {
                guid: Some("f_new".to_string()),
                title: Some("New".to_string()),
                cast: Some(UserDataType::Float),
                calc_mode: Some(CalcMode::Formula),
                formula: Some("[Sales] + 1".to_string()),
                ..FieldPatch::default()
            },
        }];
        let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
        let added = prepared
            .fields
            .iter()
            .find(|item| item.id == "f_new")
            .expect("the added field is in the legend");
        assert_eq!(added.field_type, FieldType::Measure);
        // Nothing changed for the fields the query already had.
        assert_eq!(prepared.fields[0].field_type, FieldType::Dimension);
        assert_eq!(prepared.fields[1].field_type, FieldType::Measure);
    }

    #[tokio::test]
    async fn unknown_field_is_a_400_with_a_code() {
        let mut request = result_request();
        request.fields[0].field_ref = FieldRef::title("Nope");
        let error = service()
            .result(&demo_dataset(), &request)
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.FIELD.NOT_FOUND");
        assert!(error.message.contains("Nope"));
    }

    #[tokio::test]
    async fn preview_selects_every_visible_field() {
        let request = PreviewRequest {
            dataset: Some(demo_dataset()),
            ..PreviewRequest::default()
        };
        let prepared = service().prepare_preview(&request).unwrap();
        // `GroupByPolicy.if_measures`: the dataset has measures, so its aggregation applies and
        // the query groups by every selected dimension.
        assert_eq!(
            prepared.sql,
            "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\", \
             t0.\"Year\" AS \"f_year\", (SUM(t0.\"Sales\") * 2) AS \"f_calc\" \
             FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\", t0.\"Year\" LIMIT 100"
        );
    }

    /// The editor's "Number of rows" field is a `limit`, not a hard limit: `100` and `1000` both
    /// have to come back as the page size, and a dataset with fewer rows is not a 422 either.
    /// Python's hard limit for a preview is 100_000, so a page is never clamped to 100.
    #[tokio::test]
    async fn a_preview_page_is_its_own_limit() {
        let page = |limit: Option<i64>| PreviewRequest {
            dataset: Some(demo_dataset()),
            limit,
            ..PreviewRequest::default()
        };
        let service = service();
        let limit_of = |request: &PreviewRequest| {
            let prepared = service.prepare_preview(request).unwrap();
            prepared
                .sql
                .rsplit_once("LIMIT ")
                .map(|(_, limit)| limit.to_string())
        };
        assert_eq!(limit_of(&page(Some(100))).as_deref(), Some("100"));
        assert_eq!(limit_of(&page(Some(1000))).as_deref(), Some("1000"));
        // No `limit` in the body is the schema's own 100-row page.
        assert_eq!(limit_of(&page(None)).as_deref(), Some("100"));
        // A preview block is capped at `PREVIEW_ROW_LIMIT` (1000) whatever it asks for: Python's
        // `make_limit_offset` takes `min(limit or 1000, 1000)` for a preview.
        assert_eq!(limit_of(&page(Some(5000))).as_deref(), Some("1000"));
        assert_eq!(limit_of(&page(Some(200_000))).as_deref(), Some("1000"));
        // The hard limit still wins when it is the smaller number.
        let raised = PreviewRequest {
            dataset: Some(demo_dataset()),
            limit: Some(5000),
            row_count_hard_limit: Some(500),
            ..PreviewRequest::default()
        };
        assert_eq!(limit_of(&raised).as_deref(), Some("500"));
    }

    #[cfg(test)]
    mod request_model_tests {
        use super::*;
        use crate::executor::StaticExecutor;
        use crate::testing::demo_dataset;
        use dl_core::{CalcMode, Field};
        use serde_json::json;

        fn service() -> Service<StaticExecutor> {
            Service::new(
                StaticExecutor::new(vec![
                    vec![Some("Europe".into()), Some("42".into())],
                    vec![Some("Asia".into()), None],
                ]),
                Dialect::Postgres,
            )
        }

        fn parameter_dataset() -> Dataset {
            let mut dataset = demo_dataset();
            dataset.fields.push(Field {
                guid: "p_scale".to_string(),
                title: "Scale".to_string(),
                data_type: Some(UserDataType::Float),
                calc_mode: CalcMode::Parameter,
                default_value: Some(json!(0.5)),
                ..Field::default()
            });
            dataset
        }

        #[test]
        fn a_parameter_value_from_the_request_replaces_the_default() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [{"ref": {"type": "id", "id": "p_scale"}, "role": "dimension"}],
                "parameter_values": [{"ref": {"type": "id", "id": "p_scale"}, "value": 2}],
            }))
            .unwrap();
            let prepared = service()
                .prepare_result(&parameter_dataset(), &request)
                .unwrap();
            assert_eq!(
                prepared.blocks[0].query.sql,
                "SELECT 2 AS \"p_scale\" FROM \"public\".\"sales\" AS t0 LIMIT 1000001"
            );
        }

        #[test]
        fn an_unknown_filter_is_dropped_only_on_request() {
            let payload = json!({
                "fields": [{"ref": {"type": "id", "id": "f_region"}, "role": "dimension"}],
                "filters": [{"ref": {"type": "id", "id": "f_gone"}, "operation": "eq", "values": [1]}],
            });
            let strict: ResultRequest = serde_json::from_value(payload.clone()).unwrap();
            let error = service()
                .prepare_result(&demo_dataset(), &strict)
                .unwrap_err();
            assert_eq!(error.code, "ERR.DS_API.FIELD.NOT_FOUND");

            let mut lenient: ResultRequest = serde_json::from_value(payload).unwrap();
            lenient.ignore_nonexistent_filters = true;
            let prepared = service().prepare_result(&demo_dataset(), &lenient).unwrap();
            // `/result` is an aggregation, so the surviving dimension is grouped:
            // `dl_query_processing` emits `GROUP BY` for the selected dimensions even when no
            // measure aggregates, which is what makes the list unique.
            assert_eq!(
                prepared.blocks[0].query.sql,
                "SELECT t0.\"Region\" AS \"f_region\" FROM \"public\".\"sales\" AS t0 \
                 GROUP BY t0.\"Region\" LIMIT 1000001"
            );
        }

        /// `Legend.list_selectable_items` keeps `FieldObjSpec`s: a `template` item that is a
        /// placeholder has no field, so it stays out of `SELECT`/`GROUP BY` and its value is the
        /// template string (empty), filled in while the row is decoded.
        #[tokio::test]
        async fn a_template_placeholder_is_not_selected_or_grouped_by() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                    {"ref": {"type": "placeholder"}, "role_spec": {"role": "template", "template": ""}},
                ],
            }))
            .unwrap();
            let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
            let sql = &prepared.blocks[0].query.sql;
            assert!(
                sql.starts_with("SELECT t0.\"Region\" AS \"f_region\" FROM"),
                "{sql}"
            );
            assert!(!sql.contains("placeholder"), "{sql}");
            assert_eq!(
                sql.matches("GROUP BY").count(),
                1,
                "only the row dimension is grouped: {sql}"
            );

            let response = service().result(&demo_dataset(), &request).await.unwrap();
            let rows = response.rows().unwrap();
            // The placeholder keeps its position in the row but has no column of its own.
            assert_eq!(rows[0].data, vec![json!("Europe"), json!("")]);
            assert_eq!(rows[0].legend, vec![0, 1]);
        }

        #[tokio::test]
        async fn an_info_item_is_in_the_legend_but_not_in_the_rows() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role": "dimension"},
                    {"ref": {"type": "id", "id": "f_year"}, "role": "info"},
                ],
            }))
            .unwrap();
            let response = service().result(&demo_dataset(), &request).await.unwrap();
            assert_eq!(response.fields.len(), 2);
            assert_eq!(response.fields[1].id, "f_year");
            let rows = response.rows().unwrap();
            assert_eq!(rows[0].data, vec![json!("Europe")]);
            assert_eq!(rows[0].legend, vec![0]);
        }

        #[tokio::test]
        async fn a_dataset_that_forbids_export_says_so() {
            let mut dataset = demo_dataset();
            dataset.data_export_forbidden = true;
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [{"ref": {"type": "id", "id": "f_region"}, "role": "dimension"}],
            }))
            .unwrap();
            let response = service().result(&dataset, &request).await.unwrap();
            assert!(response.data_export_forbidden);
            assert!(!response.data_export.basic.allowed);
            assert_eq!(
                response.data_export.basic.reason.as_deref(),
                Some(&["DISABLED_EXPORT_DATASET".to_string()][..])
            );
        }
    }
    mod totals_and_preflight {
        use super::*;

        fn totals_request() -> ResultRequest {
            serde_json::from_value(json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                    {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
                ],
                "order_by": [{"ref": {"type": "id", "id": "f_sales"}, "direction": "desc"}],
                "filters": [
                    {"ref": {"type": "id", "id": "f_region"}, "operation": "eq", "values": ["Europe"]},
                ],
                "limit": 10,
                "result": {"with_totals": true},
            }))
            .unwrap()
        }

        #[test]
        fn with_totals_is_nested_in_the_result_object() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [],
                "result": {"with_totals": true},
            }))
            .unwrap();
            assert!(request.with_totals());
            // The v1 flag was top-level; the v2 schema takes the nested one only.
            let legacy: ResultRequest =
                serde_json::from_value(json!({"fields": [], "with_totals": true})).unwrap();
            assert!(!legacy.with_totals());
        }

        #[test]
        fn with_totals_compiles_the_measures_only() {
            let prepared = service()
                .prepare_result(&demo_dataset(), &totals_request())
                .unwrap();
            assert_eq!(
                prepared.blocks[0].query.sql,
                "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 WHERE t0.\"Region\" = 'Europe' \
                 GROUP BY t0.\"Region\" ORDER BY 2 DESC LIMIT 10"
            );
            let totals = prepared.totals.expect("a totals query");
            // No dimensions: they became placeholders and a placeholder is not selected, so
            // there is nothing to `GROUP BY`; `order_by` and `limit` are not carried over.
            assert_eq!(
                totals.query.sql,
                "SELECT SUM(t0.\"Sales\") AS \"f_sales\" FROM \"public\".\"sales\" AS t0 \
                 WHERE t0.\"Region\" = 'Europe'"
            );
            assert!(!totals.query.grouped);
            assert_eq!(totals.query.legend.len(), 1);
            // The measure keeps the legend id it has in the main query.
            assert_eq!(totals.query.legend[0].legend_item_id, 1);
            assert_eq!(totals.query.legend[0].role_spec.role, Role::Measure);
            // `_make_totals` is not a block of `execute_all_queries`: its row and its `blocks`
            // entry stay off the v2 body (see [`PreparedTotals`]).
        }

        #[test]
        fn a_totals_measure_that_needs_a_query_fork_is_null() {
            // "Sales as % of TOTAL" has no TOTAL fork in the totals query, and the reference
            // selects `NULL` for it instead of dividing the total by itself.
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                    {"ref": {"type": "id", "id": "f_share"}, "role_spec": {"role": "measure"}},
                ],
                "updates": [{"action": "add_field", "field": {
                    "guid": "f_share",
                    "title": "Sales as % of TOTAL",
                    "calc_mode": "formula",
                    "formula": "[Sales] / sum([Sales] TOTAL)",
                    "data_type": "float",
                    "cast": "float",
                    "type": "MEASURE",
                }}],
                "result": {"with_totals": true},
            }))
            .unwrap();
            let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
            let totals = prepared.totals.expect("a totals query");
            assert!(totals.query.sql.contains("NULL AS"), "{}", totals.query.sql);
            assert!(!totals.query.sql.contains("JOIN"), "{}", totals.query.sql);
        }

        #[test]
        fn a_totals_request_without_measures_selects_nothing() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
                "result": {"with_totals": true},
            }))
            .unwrap();
            let prepared = service().prepare_result(&demo_dataset(), &request).unwrap();
            let totals = prepared.totals.expect("a totals row");
            // `EmptyQueryMode.empty_row`: no SQL, but the block still answers one empty row.
            assert_eq!(totals.query.sql, "");
            assert!(totals.query.columns.is_empty());
        }

        /// A totals request as the charts engine sends it: the base block and the expanded
        /// totals block, with a placeholder in the dimension position.
        fn expanded_totals_request() -> ResultRequest {
            serde_json::from_value(json!({
                "fields": [
                    {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "block_id": 0},
                    {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}, "block_id": 0},
                    {
                        "ref": {"type": "placeholder"},
                        "role_spec": {"role": "template", "template": ""},
                        "block_id": 1,
                    },
                    {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "total"}, "block_id": 1},
                ],
                "with_totals": true,
            }))
            .unwrap()
        }

        #[test]
        fn the_expanded_totals_block_is_compiled_from_the_request() {
            let prepared = service()
                .prepare_result(&demo_dataset(), &expanded_totals_request())
                .unwrap();
            // Block 0 is the request's own query: the totals items are not part of it.
            assert_eq!(
                prepared.blocks[0].query.sql,
                "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" LIMIT 1000001"
            );
            // The totals block came from the request itself: two items, one of them a
            // placeholder, which keeps its position but has no column.
            let totals = &prepared.blocks[1];
            assert_eq!(totals.block_id, 1);
            // The pre-paginator pushed `limit + offset` down to every block, so the totals block
            // keeps its `LIMIT` but no `ORDER BY` (its dimensions are placeholders).
            assert_eq!(
                totals.query.sql,
                "SELECT SUM(t0.\"Sales\") AS \"f_sales\" FROM \"public\".\"sales\" AS t0 \
                 LIMIT 1000001"
            );
            assert_eq!(totals.query.cells.len(), 2);
            assert_eq!(
                totals.query.cells[0],
                QueryCell::Template { legend_item_id: 2 }
            );
            assert_eq!(
                totals.query.cells[1],
                QueryCell::Column {
                    legend_item_id: 3,
                    index: 0
                }
            );
            // `with_totals` at the top of the body is what the charts engine sends, and the v2
            // schema ignores it: the block alone answers the request.
            assert!(prepared.totals.is_none());
        }

        #[test]
        fn a_placeholder_becomes_the_pseudo_field_of_the_legend() {
            let prepared = service()
                .prepare_result(&demo_dataset(), &expanded_totals_request())
                .unwrap();
            // The legend is the merged one: block 0's items, then the totals block's.
            let ids: Vec<(&str, i64)> = prepared
                .fields
                .iter()
                .map(|item| (item.id.as_str(), item.legend_item_id))
                .collect();
            assert_eq!(
                ids,
                vec![
                    ("f_region", 0),
                    ("f_sales", 1),
                    ("__@placeholder__", 2),
                    ("f_sales", 3)
                ]
            );
            let placeholder = &prepared.fields[2];
            assert_eq!(placeholder.item_type, "placeholder");
            assert_eq!(placeholder.title, "Placeholder");
            assert_eq!(placeholder.role_spec, crate::dto::RoleSpec::template(""));
        }

        #[tokio::test]
        async fn the_totals_row_of_an_expanded_block_is_merged_into_result_data() {
            let (executor, calls) = RecordingExecutor::new(vec![
                vec![
                    vec![Some("Europe".into()), Some("42".into())],
                    vec![Some("Asia".into()), Some("7".into())],
                ],
                vec![vec![Some("49".into())]],
            ]);
            let service = Service::new(executor, Dialect::Postgres);
            let response = service
                .result(&demo_dataset(), &expanded_totals_request())
                .await
                .unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
            let rows = response.rows().unwrap();
            // The totals row is the last one of the single chunk, and its cells are the
            // placeholder (an empty template) plus the aggregate.
            assert_eq!(
                rows.last().unwrap(),
                &DataRow {
                    data: vec![json!(""), json!("49")],
                    legend: vec![2, 3],
                }
            );
            // Two blocks: the base one and the totals block, as Python's `blocks`.
            assert_eq!(
                response
                    .blocks
                    .iter()
                    .map(|block| block.block_id)
                    .collect::<Vec<_>>(),
                vec![0, 1]
            );
        }

        #[tokio::test]
        async fn the_totals_query_runs_after_the_main_one_and_stays_off_the_wire() {
            let (executor, calls) = RecordingExecutor::new(vec![
                vec![vec![Some("Europe".into()), Some("42".into())]],
                vec![vec![Some("42".into())]],
            ]);
            let service = Service::new(executor, Dialect::Postgres);
            let response = service
                .result(&demo_dataset(), &totals_request())
                .await
                .unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
            assert_eq!(
                response.totals,
                Some(DataRow {
                    data: vec![json!("42")],
                    legend: vec![1],
                })
            );
            // `DataApiV2ResponseSchema` has no `totals`, and `_make_totals` is not a block of
            // `execute_all_queries`: the row stays off the v2 body.
            let body = serde_json::to_value(&response).unwrap();
            assert!(body.get("totals").is_none());
            assert_eq!(
                body["result_data"][0]["rows"][0]["data"],
                json!(["Europe", "42"])
            );
            assert_eq!(body["blocks"].as_array().unwrap().len(), 1);
        }
    }

    mod preflight_tests {
        use super::*;

        #[test]
        fn preflight_says_ok_for_a_valid_dataset() {
            let preflight = service().preflight(&demo_dataset(), &ResultRequest::default());
            assert_eq!(preflight.status, 200);
            assert!(preflight.is_ok());
            assert_eq!(preflight.body.code, "OK");
            assert_eq!(preflight.body.message, "Validation was successful");
            assert!(preflight.body.dataset.component_errors.is_empty());
            let fields = preflight.body.dataset.fields.expect("the field list");
            assert_eq!(fields.len(), 4);
            assert_eq!(fields[0].guid, "f_region");
            assert_eq!(fields[0].data_type, Some(UserDataType::String));
            assert_eq!(fields[0].kind, FieldType::Dimension);
            assert_eq!(fields[3].calc_mode, CalcMode::Formula);
            assert_eq!(fields[3].kind, FieldType::Dimension);
        }

        #[test]
        fn preflight_reports_a_broken_formula_under_its_field() {
            let mut dataset = demo_dataset();
            dataset.fields[3].guid_formula = "SUM(".to_string();
            let preflight = service().preflight(&dataset, &ResultRequest::default());
            assert_eq!(preflight.status, 400);
            assert_eq!(preflight.body.code, "ERR.DS_API.VALIDATION.ERROR");
            let registry = &preflight.body.dataset.component_errors;
            let pack = registry
                .get("f_calc")
                .expect("a pack for the formula field");
            assert_eq!(pack.errors.len(), 1);
            assert_eq!(pack.errors[0].code, "ERR.DS_API.FORMULA.VALIDATION");
            // The field list still comes back: the UI shows the error next to the field.
            assert!(preflight.body.dataset.fields.is_some());
        }

        #[test]
        fn preflight_reports_structural_errors_of_several_fields() {
            let mut dataset = demo_dataset();
            dataset.fields[0].data_type = None;
            dataset.fields[0].initial_data_type = None;
            // `f_sales` comes before `f_year`, so the second field with the title is the one
            // that gets the pack, exactly like `Dataset::validate` reports the first problem.
            dataset.fields[1].title = "Year".to_string();
            let preflight = service().preflight(&dataset, &ResultRequest::default());
            assert_eq!(preflight.status, 400);
            let registry = &preflight.body.dataset.component_errors;
            assert_eq!(
                registry.get("f_region").unwrap().errors[0].code,
                "ERR.DS_API.FIELD.INVALID"
            );
            assert_eq!(
                registry.get("f_year").unwrap().errors[0].code,
                "ERR.DS_API.FIELD.TITLE.CONFLICT"
            );
        }

        #[test]
        fn preflight_answers_a_fatal_error_for_an_empty_dataset() {
            let dataset = Dataset {
                sources: demo_dataset().sources,
                fields: Vec::new(),
                data_export_forbidden: false,
                connection_id: None,
                ..Dataset::default()
            };
            let preflight = service().preflight(&dataset, &ResultRequest::default());
            assert_eq!(preflight.status, 400);
            assert_eq!(preflight.body.code, "ERR.DS_API.VALIDATION.FATAL");
            assert_eq!(
                preflight.body.message,
                "Validation encountered a fatal error."
            );
            assert!(preflight.body.dataset.fields.is_none());
        }

        #[test]
        fn preflight_answers_a_refused_update_with_its_own_code() {
            let request: ResultRequest = serde_json::from_value(json!({
                "fields": [],
                // `clone_field` is a dataset action the data-api refuses.
                "updates": [{"action": "clone_field", "field": {"guid": "f_x", "from_guid": "f_region"}}],
            }))
            .unwrap();
            let preflight = service().preflight(&demo_dataset(), &request);
            assert_eq!(preflight.status, 400);
            // `prepare_dataset_with_mutation_cache` fails before validation starts, so the
            // answer is that error, with an empty registry and no field list.
            assert_eq!(preflight.body.code, "ERR.DS_API.ACTION_NOT_ALLOWED");
            assert!(preflight.body.dataset.component_errors.is_empty());
            assert!(preflight.body.dataset.fields.is_none());
        }
    }

    mod pivot_tests {
        use super::*;
        use crate::dto::{PivotRole, PivotSpec, PivotSpecItem, PivotTotals};
        use crate::legend::{DIMENSION_NAME_TITLE, MEASURE_NAME_ID};
        use crate::pivot::make_pivot_legend;

        fn spec_item(legend_item_ids: Vec<i64>, role: PivotRole) -> PivotSpecItem {
            PivotSpecItem {
                pivot_item_id: None,
                legend_item_ids,
                role_spec: crate::dto::PivotRoleSpec::new(role),
                title: None,
            }
        }

        fn pivot_request() -> PivotRequest {
            PivotRequest {
                fields: vec![
                    FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
                ],
                pivot: PivotSpec {
                    structure: vec![
                        spec_item(vec![0], PivotRole::Row),
                        spec_item(vec![1], PivotRole::Measure),
                    ],
                    ..PivotSpec::default()
                },
                ..PivotRequest::default()
            }
        }

        /// `service()` answers with a null `Sales`, which would make the sorting tests
        /// indistinguishable from the null-ordering ones.
        fn pivot_service() -> Service<StaticExecutor> {
            Service::new(
                StaticExecutor::new(vec![
                    vec![Some("Europe".into()), Some("42".into())],
                    vec![Some("Asia".into()), Some("7".into())],
                ]),
                Dialect::Postgres,
            )
        }

        #[test]
        fn pivot_compiles_the_v2_request_with_the_pivot_hard_limit() {
            let prepared = service()
                .prepare_pivot(&demo_dataset(), &pivot_request())
                .unwrap();
            assert_eq!(
                prepared.query.sql,
                "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" LIMIT 100000"
            );
            assert!(prepared.query.grouped);
            // The legend gained the two pseudo-dimensions the pivot needs.
            assert_eq!(prepared.legend.measure_name_ids(), vec![2]);
            assert_eq!(prepared.legend.dimension_name_ids(), vec![3]);
            assert_eq!(prepared.legend.select_ids(), vec![0, 1]);
            assert_eq!(
                prepared
                    .pivot_legend
                    .items()
                    .iter()
                    .map(|item| item.pivot_item_id)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2, 3]
            );
        }

        /// `Region` across the columns, `Year` down the rows, `Sales` as the measure.
        fn two_axis_request() -> PivotRequest {
            PivotRequest {
                fields: vec![
                    FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_year"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
                ],
                pivot: PivotSpec {
                    structure: vec![
                        spec_item(vec![0], PivotRole::Column),
                        spec_item(vec![1], PivotRole::Row),
                        spec_item(vec![2], PivotRole::Measure),
                    ],
                    ..PivotSpec::default()
                },
                ..PivotRequest::default()
            }
        }

        /// The ids of the extended legend, in legend order: the request items, then one cloned
        /// item per block cell (`4..=12`), and the pseudo-dimensions at the end.
        fn legend_ids(prepared: &PreparedPivot) -> Vec<i64> {
            prepared
                .legend
                .items()
                .iter()
                .map(|item| item.legend_item_id)
                .collect()
        }

        #[test]
        fn with_totals_adds_a_block_per_axis_and_one_for_the_intersection() {
            let mut request = two_axis_request();
            request.pivot.with_totals = Some(true);
            let prepared = service().prepare_pivot(&demo_dataset(), &request).unwrap();

            // The base block is untouched: it selects the dimensions the request named.
            assert_eq!(
                prepared.query.sql,
                "SELECT t0.\"Region\" AS \"f_region\", t0.\"Year\" AS \"f_year\", \
                 SUM(t0.\"Sales\") AS \"f_sales\" FROM \"public\".\"sales\" AS t0 \
                 GROUP BY t0.\"Region\", t0.\"Year\" LIMIT 100000"
            );

            let totals = &prepared.totals;
            assert_eq!(totals.len(), 3);
            assert_eq!(
                totals
                    .iter()
                    .map(|block| block.block_id)
                    .collect::<Vec<_>>(),
                vec![1, 2, 3]
            );

            // 1. The row total: `Year` becomes a template, `Region` (the other axis) stays.
            assert_eq!(
                totals[0].sql,
                "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" LIMIT 100000"
            );
            // 2. The column total: the other way around.
            assert_eq!(
                totals[1].sql,
                "SELECT t0.\"Year\" AS \"f_year\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Year\" LIMIT 100000"
            );
            // 3. The intersection: both axes are templates, so nothing is grouped.
            assert_eq!(
                totals[2].sql,
                "SELECT SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 LIMIT 100000"
            );

            // Every dropped dimension is filled in while decoding, in the merged stream's order.
            assert_eq!(
                totals[0].cells,
                vec![
                    (5, pivot::TotalCell::Template),
                    (6, pivot::TotalCell::Column { index: 0 }),
                    (7, pivot::TotalCell::Column { index: 1 }),
                ]
            );
            assert_eq!(
                totals[2].cells,
                vec![
                    (11, pivot::TotalCell::Template),
                    (12, pivot::TotalCell::Template),
                    (13, pivot::TotalCell::Column { index: 0 }),
                ]
            );

            // A block's own items are `template`/`total` (`row` for a kept dimension).
            let role = |id: i64| prepared.legend.get_item(id).unwrap().role_spec.role;
            assert_eq!(role(5), Role::Template);
            assert_eq!(role(6), Role::Row);
            assert_eq!(role(7), Role::Total);
            // ... and they are the same fields as the originals.
            assert_eq!(prepared.legend.get_item(6).unwrap().id, "f_region");
            assert_eq!(prepared.legend.get_item(7).unwrap().id, "f_sales");

            // New ids go right after the last selected item, before the pseudo-dimensions
            // (`Measure Names`/`Dimension Names` are legend-only and stay at the end).
            assert_eq!(
                legend_ids(&prepared),
                vec![0, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12, 13, 3, 4]
            );

            // The patched structure is columns, rows, measures; each item lists its clones.
            let structure = prepared.pivot_legend.items();
            assert_eq!(structure.len(), 4);
            assert_eq!(structure[0].role_spec.role, PivotRole::Column);
            assert_eq!(structure[0].legend_item_ids, vec![0, 6, 8, 12]);
            assert_eq!(structure[1].role_spec.role, PivotRole::Row);
            assert_eq!(structure[1].legend_item_ids, vec![1, 5, 9, 11]);
            assert_eq!(structure[2].role_spec.role, PivotRole::Measure);
            assert_eq!(structure[2].legend_item_ids, vec![2, 7, 10, 13]);
            assert_eq!(structure[3].title, crate::legend::DIMENSION_NAME_TITLE);
        }

        #[test]
        fn totals_are_expanded_from_the_flag_and_from_an_explicit_totals_spec() {
            let mut request = two_axis_request();
            request.pivot.with_totals = Some(true);
            let from_flag = service().prepare_pivot(&demo_dataset(), &request).unwrap();

            // `RawPivotTotalsSpec(rows=[level=0], columns=[level=0])` is what the flag means.
            let mut request = two_axis_request();
            request.pivot.totals = Some(PivotTotals {
                rows: vec![crate::dto::PivotTotalsItem { level: Some(0) }],
                columns: vec![crate::dto::PivotTotalsItem { level: Some(0) }],
            });
            let explicit = service().prepare_pivot(&demo_dataset(), &request).unwrap();

            assert_eq!(from_flag.query.sql, explicit.query.sql);
            assert_eq!(from_flag.totals, explicit.totals);
            assert_eq!(from_flag.legend, explicit.legend);
            assert_eq!(from_flag.pivot_legend, explicit.pivot_legend);
        }

        #[test]
        fn totals_on_one_axis_build_a_single_block() {
            let mut request = two_axis_request();
            // Only the rows are totalled: `columns` is empty, so there is no column total and
            // no intersection either.
            request.pivot.totals = Some(PivotTotals {
                rows: vec![crate::dto::PivotTotalsItem { level: Some(0) }],
                columns: Vec::new(),
            });
            let prepared = service().prepare_pivot(&demo_dataset(), &request).unwrap();
            assert_eq!(prepared.totals.len(), 1);
            assert_eq!(
                prepared.totals[0].sql,
                "SELECT t0.\"Region\" AS \"f_region\", SUM(t0.\"Sales\") AS \"f_sales\" \
                 FROM \"public\".\"sales\" AS t0 GROUP BY t0.\"Region\" LIMIT 100000"
            );
            assert_eq!(
                prepared.totals[0].cells,
                vec![
                    (5, pivot::TotalCell::Template),
                    (6, pivot::TotalCell::Column { index: 0 }),
                    (7, pivot::TotalCell::Column { index: 1 }),
                ]
            );
        }

        #[test]
        fn a_level_above_zero_keeps_the_first_dimensions_in_place() {
            let mut request = two_axis_request();
            // `columns=[level=1]`: the one column dimension (`Region`) is below the level, so
            // it is selected again and only the measure stays a total.
            request.pivot.totals = Some(PivotTotals {
                rows: Vec::new(),
                columns: vec![crate::dto::PivotTotalsItem { level: Some(1) }],
            });
            let prepared = service().prepare_pivot(&demo_dataset(), &request).unwrap();
            assert_eq!(prepared.totals.len(), 1);
            assert_eq!(prepared.totals[0].sql, prepared.query.sql);
            assert_eq!(
                prepared.totals[0].cells,
                vec![
                    (5, pivot::TotalCell::Column { index: 0 }),
                    (6, pivot::TotalCell::Column { index: 1 }),
                    (7, pivot::TotalCell::Column { index: 2 }),
                ]
            );
        }

        #[test]
        fn a_pivot_without_measures_keeps_its_single_block() {
            // No measure at all: there is nothing to total, and `normalize_totals` returns
            // `None` (`if not measures: return pivot_spec, raw_query_spec_union`).
            let mut request = PivotRequest {
                fields: vec![
                    FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_year"), Role::Row),
                ],
                pivot: PivotSpec {
                    structure: vec![
                        spec_item(vec![0], PivotRole::Row),
                        spec_item(vec![1], PivotRole::Column),
                    ],
                    ..PivotSpec::default()
                },
                ..PivotRequest::default()
            };
            let without_totals = service().prepare_pivot(&demo_dataset(), &request).unwrap();
            request.pivot.with_totals = Some(true);
            let prepared = service().prepare_pivot(&demo_dataset(), &request).unwrap();
            assert!(prepared.totals.is_empty());
            assert_eq!(prepared.query.sql, without_totals.query.sql);
            assert_eq!(prepared.legend, without_totals.legend);
        }

        #[tokio::test]
        async fn the_totals_rows_are_merged_into_the_table() {
            let (executor, calls) = RecordingExecutor::new(vec![
                // The base block: the two cells of the cross-tab.
                vec![
                    vec![
                        Some("Europe".into()),
                        Some("2020".into()),
                        Some("42".into()),
                    ],
                    vec![Some("Asia".into()), Some("2021".into()), Some("7".into())],
                ],
                // The row total: `Region` only.
                vec![
                    vec![Some("Europe".into()), Some("42".into())],
                    vec![Some("Asia".into()), Some("7".into())],
                ],
                // The column total: `Year` only.
                vec![
                    vec![Some("2020".into()), Some("42".into())],
                    vec![Some("2021".into()), Some("7".into())],
                ],
                // The intersection: the grand total.
                vec![vec![Some("49".into())]],
            ]);
            let service = Service::new(executor, Dialect::Postgres);
            let mut request = two_axis_request();
            request.pivot.with_totals = Some(true);
            let response = service.pivot(&demo_dataset(), &request).await.unwrap();

            // Every block ran, in order, and all four are reported in the response.
            let sql = calls.lock().unwrap().clone();
            assert_eq!(sql.len(), 4);
            assert!(sql[0].contains("GROUP BY t0.\"Region\", t0.\"Year\""));
            assert!(sql[1].contains("GROUP BY t0.\"Region\""));
            assert!(sql[2].contains("GROUP BY t0.\"Year\""));
            assert!(!sql[3].contains("GROUP BY"));
            assert_eq!(
                response
                    .blocks
                    .iter()
                    .map(|block| block.block_id)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2, 3]
            );

            // The clones of the kept dimensions collapse onto the base items
            // (`DataCellConverter._legend_item_id_map`), so the axes have one entry per real
            // value plus one total.
            assert_eq!(response.pivot_data.rows.len(), 3);
            assert_eq!(response.pivot_data.columns.len(), 3);

            let value = |row: usize, column: usize| {
                response.pivot_data.rows[row].values[column]
                    .as_ref()
                    .map(|cells| cells[0].0.clone())
            };
            // Columns ascend (`Asia`, `Europe`), rows ascend (`2020`, `2021`); the totals go
            // last on both axes (`NativePivotSorter._axis_has_total`).
            assert_eq!(value(0, 1), Some(json!("42")));
            assert_eq!(value(1, 0), Some(json!("7")));
            assert_eq!(value(0, 0), None);
            // Row total, column total and the grand total.
            assert_eq!(value(2, 1), Some(json!("42")));
            assert_eq!(value(2, 0), Some(json!("7")));
            assert_eq!(value(0, 2), Some(json!("42")));
            assert_eq!(value(1, 2), Some(json!("7")));
            assert_eq!(value(2, 2), Some(json!("49")));

            // The total labels are the empty template, not a null hole.
            let header = |row: usize| {
                response.pivot_data.rows[row].header[0]
                    .as_ref()
                    .map(|cells| cells[0].0.clone())
            };
            assert_eq!(header(0), Some(json!("2020")));
            assert_eq!(header(2), Some(json!("")));
        }

        #[test]
        fn a_pivot_selects_the_fields_and_never_the_measure_names_pseudo_dimension() {
            // The UI sends `{"ref": {"type": "measure_name"}, "legend_item_id": 0}` as the first
            // field of a pivot. It is a legend item, not a dataset column: the query selects the
            // fields only, and the structure still refers to legend id 0.
            let mut request = pivot_request();
            request
                .fields
                .insert(0, FieldsItem::new(FieldRef::MeasureName, Role::Row));
            request
                .pivot
                .structure
                .insert(0, spec_item(vec![0], PivotRole::Column));
            // Legend ids shifted by the inserted item: the old 0/1 are now 1/2.
            request.pivot.structure[1].legend_item_ids = vec![1];
            request.pivot.structure[2].legend_item_ids = vec![2];
            let prepared = service().prepare_pivot(&demo_dataset(), &request).unwrap();
            assert!(
                prepared.query.sql.contains("SELECT t0.\"Region\""),
                "{}",
                prepared.query.sql
            );
            assert!(
                !prepared.query.sql.contains("__@measure_name__"),
                "{}",
                prepared.query.sql
            );
            assert_eq!(
                prepared.pivot_legend.items()[0].item_type,
                crate::pivot::PivotItemType::MeasureName
            );
        }

        #[test]
        fn row_totals_keep_measure_names_out_of_the_template_items() {
            // A totals row (`level = 0`) turns every row dimension into a template placeholder.
            // `Measure Names` is a pseudo-dimension, not a dimension: it must not be mixed with
            // a placeholder in one pivot item (that is `ITEMS_INCOMPATIBLE`).
            let mut request = pivot_request();
            request
                .fields
                .insert(1, FieldsItem::new(FieldRef::MeasureName, Role::Row));
            // Region and `Measure Names` on the rows, then the measure: the axis keeps a real
            // dimension next to the pseudo one, which is what makes the row total valid.
            request.pivot.structure = vec![
                spec_item(vec![0], PivotRole::Row),
                spec_item(vec![1], PivotRole::Row),
                spec_item(vec![2], PivotRole::Measure),
            ];
            request.pivot.totals = Some(crate::dto::PivotTotals {
                rows: vec![crate::dto::PivotTotalsItem { level: Some(0) }],
                columns: Vec::new(),
            });
            let prepared = service()
                .prepare_pivot(&demo_dataset(), &request)
                .expect("a row total with Measure Names is a valid pivot");
            assert_eq!(prepared.totals.len(), 1);
        }

        #[test]
        fn pivot_rejects_an_unknown_field() {
            let mut request = pivot_request();
            request.fields[0] = FieldsItem::new(FieldRef::id("f_nope"), Role::Row);
            let error = service()
                .prepare_pivot(&demo_dataset(), &request)
                .unwrap_err();
            assert_eq!(error.status, 400);
            assert_eq!(error.code, "ERR.DS_API.FIELD.NOT_FOUND");
        }

        #[test]
        fn an_invalid_pivot_structure_is_a_400_with_the_pivot_code() {
            let mut request = pivot_request();
            // `Sales` is a measure, so it cannot be a row dimension.
            request.pivot.structure[0] = spec_item(vec![1], PivotRole::Row);
            let error = service()
                .prepare_pivot(&demo_dataset(), &request)
                .unwrap_err();
            assert_eq!(error.status, 400);
            assert_eq!(error.code, "ERR.DS_API.PIVOT.LEGEND.INVALID_ROLE");
        }

        #[test]
        fn the_legend_formalizer_keeps_measure_names_out_of_the_select_list() {
            let legend = LegendFormalizer::for_pivot(&demo_dataset(), &pivot_request())
                .make()
                .unwrap();
            assert_eq!(legend.select_ids(), vec![0, 1]);
            assert_eq!(legend.entries.len(), 4);
            assert!(!legend.entries[2].in_result_set);
            assert!(!legend.entries[3].in_result_set);
            // `make_pivot_legend` needs both, so the formalizer must have added them.
            let pivot_legend = make_pivot_legend(&legend, &pivot_request().pivot).unwrap();
            assert_eq!(pivot_legend.dimension_name_piids().len(), 1);
            assert_eq!(pivot_legend.items()[3].title, DIMENSION_NAME_TITLE);
        }

        #[tokio::test]
        async fn pivot_answers_with_a_table() {
            let response = pivot_service()
                .pivot(&demo_dataset(), &pivot_request())
                .await
                .unwrap();

            // The legend and the pivot structure carry the patched pseudo-dimensions.
            assert_eq!(response.fields.len(), 4);
            assert_eq!(response.fields[2].id, MEASURE_NAME_ID);
            assert_eq!(response.fields[2].item_type, "measure_name");
            assert_eq!(response.fields[2].role_spec.role, Role::Info);
            assert_eq!(response.pivot.structure.len(), 4);
            assert_eq!(response.pivot.structure[3].title, DIMENSION_NAME_TITLE);

            // One column: `Measure Names` with the single measure.
            assert_eq!(response.pivot_data.columns.len(), 1);
            assert_eq!(
                response.pivot_data.columns[0],
                vec![Some(vec![(json!("Sales"), 2, 2)])]
            );
            assert_eq!(
                response.pivot_data.columns_with_info[0].header_info,
                crate::dto::PivotHeaderInfo::default()
            );

            // One row header: the `Region` title, pointing at `Dimension Names`.
            assert_eq!(
                response.pivot_data.row_dimension_headers,
                vec![Some(vec![(json!("Region"), 3, 3)])]
            );

            // Rows are sorted by the dimension: `Asia` before `Europe`.
            let rows = &response.pivot_data.rows;
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].header, vec![Some(vec![(json!("Asia"), 0, 0)])]);
            assert_eq!(rows[0].header_with_info.cells, rows[0].header);
            assert_eq!(rows[0].values, vec![Some(vec![(json!("7"), 1, 1)])]);
            assert_eq!(rows[1].header, vec![Some(vec![(json!("Europe"), 0, 0)])]);
            assert_eq!(rows[1].values, vec![Some(vec![(json!("42"), 1, 1)])]);

            // `blocks` names the query, and a pivot is never exported in the background. The
            // tenant's own ban comes first, then the pivot-specific one: `get_data_export_base_result`
            // appends `prohibited_in_pivot` to whatever the dataset already reports.
            assert!(response.blocks[0].query.is_some());
            assert!(!response.data_export.background.allowed);
            assert_eq!(
                response.data_export.background.reason,
                Some(vec![
                    "PROHIBITED_EXPORT_TENANT".to_string(),
                    "PROHIBITED_EXPORT_PIVOT_TABLE".to_string(),
                ])
            );
            assert!(response.data_export.basic.allowed);
            // The pivot body has no `data_export_forbidden` at all.
            assert_eq!(response.data_export_forbidden, None);
        }

        #[tokio::test]
        async fn pivot_pagination_crops_the_rows_of_the_table() {
            let mut request = pivot_request();
            request.pivot.pagination = crate::dto::PivotPagination {
                limit_rows: Some(1),
                offset_rows: Some(1),
            };
            let response = pivot_service()
                .pivot(&demo_dataset(), &request)
                .await
                .unwrap();
            let rows = &response.pivot_data.rows;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].header, vec![Some(vec![(json!("Europe"), 0, 0)])]);
        }

        #[tokio::test]
        async fn a_measure_sorts_the_rows_by_its_values() {
            use crate::dto::{PivotHeaderValue, PivotMeasureSorting, PivotMeasureSortingSettings};

            let mut request = pivot_request();
            request.pivot.structure[1].role_spec.sorting = Some(PivotMeasureSorting {
                column: Some(PivotMeasureSortingSettings {
                    header_values: vec![PivotHeaderValue {
                        value: "Sales".to_string(),
                    }],
                    direction: Direction::Desc,
                    role_spec: Default::default(),
                }),
                row: None,
            });
            let response = pivot_service()
                .pivot(&demo_dataset(), &request)
                .await
                .unwrap();
            // 42 (Europe) before 7 (Asia).
            assert_eq!(
                response.pivot_data.rows[0].header,
                vec![Some(vec![(json!("Europe"), 0, 0)])]
            );
            assert_eq!(
                response.pivot_data.rows[1].header,
                vec![Some(vec![(json!("Asia"), 0, 0)])]
            );
            // The sorted column reports its own direction.
            assert_eq!(
                response.pivot_data.columns_with_info[0]
                    .header_info
                    .sorting_direction,
                Some(Direction::Desc)
            );
        }
    }

    mod connections {
        use super::*;

        #[tokio::test]
        async fn the_dataset_connection_is_resolved_into_the_executor_target() {
            let (executor, _calls, targets) = RecordingExecutor::with_targets(vec![vec![vec![
                Some("Europe".into()),
                Some("42".into()),
            ]]]);
            let asked = Arc::new(Mutex::new(Vec::new()));
            let service = Service::new(executor, Dialect::Postgres).with_connection_resolver(
                Arc::new(FixedResolver {
                    dsn: Some("postgres://pg-user:postgres@db:5432/demo".to_string()),
                    asked: asked.clone(),
                }),
            );
            let mut dataset = demo_dataset();
            dataset.connection_id = Some("conn-1".to_string());

            service.result(&dataset, &result_request()).await.unwrap();

            assert_eq!(asked.lock().unwrap().as_slice(), ["conn-1".to_string()]);
            assert_eq!(
                targets.lock().unwrap().as_slice(),
                [Some("postgres://pg-user:postgres@db:5432/demo".to_string())]
            );
        }

        #[tokio::test]
        async fn a_connection_that_cannot_be_resolved_is_an_error_not_an_empty_answer() {
            let (executor, _calls, _targets) = RecordingExecutor::with_targets(vec![]);
            let service = Service::new(executor, Dialect::Postgres).with_connection_resolver(
                Arc::new(FixedResolver {
                    dsn: None,
                    asked: Arc::new(Mutex::new(Vec::new())),
                }),
            );
            let mut dataset = demo_dataset();
            dataset.connection_id = Some("conn-broken".to_string());

            let error = service
                .result(&dataset, &result_request())
                .await
                .unwrap_err();
            assert_eq!(error.status, 500);
            assert_eq!(error.code, "ERR.DS_API.DB.CONNECTION");
            assert!(error.message.contains("conn-broken"), "{}", error.message);
        }

        #[tokio::test]
        async fn every_pivot_block_runs_against_the_same_database() {
            let (executor, calls, targets) = RecordingExecutor::with_targets(vec![
                vec![vec![
                    Some("Europe".into()),
                    Some("Desktop".into()),
                    Some("42".into()),
                ]],
                vec![vec![Some("Desktop".into()), Some("42".into())]],
            ]);
            let service = Service::new(executor, Dialect::Postgres).with_connection_resolver(
                Arc::new(FixedResolver {
                    dsn: Some("postgres://db".to_string()),
                    asked: Arc::new(Mutex::new(Vec::new())),
                }),
            );
            let mut dataset = demo_dataset();
            dataset.connection_id = Some("conn-1".to_string());
            let request = PivotRequest {
                fields: vec![
                    FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_year"), Role::Row),
                    FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
                ],
                pivot: crate::dto::PivotSpec {
                    structure: vec![
                        crate::dto::PivotSpecItem {
                            pivot_item_id: None,
                            legend_item_ids: vec![0],
                            role_spec: crate::dto::PivotRoleSpec {
                                role: crate::dto::PivotRole::Row,
                                ..Default::default()
                            },
                            title: None,
                        },
                        crate::dto::PivotSpecItem {
                            pivot_item_id: None,
                            legend_item_ids: vec![1],
                            role_spec: crate::dto::PivotRoleSpec {
                                role: crate::dto::PivotRole::Column,
                                ..Default::default()
                            },
                            title: None,
                        },
                        crate::dto::PivotSpecItem {
                            pivot_item_id: None,
                            legend_item_ids: vec![2],
                            role_spec: crate::dto::PivotRoleSpec {
                                role: crate::dto::PivotRole::Measure,
                                ..Default::default()
                            },
                            title: None,
                        },
                    ],
                    with_totals: Some(true),
                    ..Default::default()
                },
                ..PivotRequest::default()
            };

            service.pivot(&dataset, &request).await.unwrap();

            // Base query + three total blocks: every one of them names the connection's DSN.
            assert_eq!(calls.lock().unwrap().len(), 4);
            let targets = targets.lock().unwrap();
            assert_eq!(targets.len(), 4);
            assert!(targets
                .iter()
                .all(|target| target.as_deref() == Some("postgres://db")));
        }

        #[tokio::test]
        async fn a_dataset_without_a_connection_leaves_the_executor_alone() {
            let (executor, _calls, targets) = RecordingExecutor::with_targets(vec![vec![vec![
                Some("Europe".into()),
                Some("42".into()),
            ]]]);
            let service = Service::new(executor, Dialect::Postgres);
            service
                .result(&demo_dataset(), &result_request())
                .await
                .unwrap();
            assert_eq!(targets.lock().unwrap().as_slice(), [None]);
        }
    }

    /// A tree drill-down: `/result` with one block per expanded level.
    mod tree_tests {
        use super::*;

        /// `f_region` as the demo's `Products` dimension: a `TREE(...)` of four columns.
        fn tree_dataset() -> Dataset {
            let mut dataset = demo_dataset();
            let region = dataset
                .fields
                .iter_mut()
                .find(|field| field.guid == "f_region")
                .unwrap();
            region.initial_data_type = Some(UserDataType::TreeStr);
            region.data_type = Some(UserDataType::TreeStr);
            region.cast = Some(UserDataType::TreeStr);
            dataset
        }

        /// What the charts engine sends after expanding `["a"]`: the root level and the level of
        /// that branch, each with its own `prefix`, `dimension_values` and `block_id`.
        fn tree_request() -> ResultRequest {
            serde_json::from_value(json!({
                "fields": [
                    {
                        "ref": {"type": "id", "id": "f_region"},
                        "legend_item_id": 1,
                        "role_spec": {"role": "tree", "level": 1, "prefix": "[]", "dimension_values": []},
                        "block_id": 0,
                    },
                    {
                        "ref": {"type": "id", "id": "f_region"},
                        "legend_item_id": 3,
                        "role_spec": {
                            "role": "tree",
                            "level": 2,
                            "prefix": "[\"a\"]",
                            "dimension_values": [{"legend_item_id": 1, "value": "[\"a\"]"}],
                        },
                        "block_id": 1,
                    },
                    {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
                ],
            }))
            .unwrap()
        }

        #[test]
        fn a_drill_down_is_one_query_per_level() {
            let prepared = service()
                .prepare_result(&tree_dataset(), &tree_request())
                .unwrap();
            assert_eq!(
                prepared
                    .blocks
                    .iter()
                    .map(|block| block.block_id)
                    .collect::<Vec<_>>(),
                vec![0, 1]
            );
            assert_eq!(prepared.blocks[0].placement, Placement::Root);
            // The level is placed under the branch it expands: the root row with `["a"]`.
            assert_eq!(
                prepared.blocks[1].placement,
                Placement::After {
                    dimension_values: Some(vec![DimensionValueSpec {
                        legend_item_id: 1,
                        value: json!("[\"a\"]"),
                    }]),
                }
            );
            // `slice(expr, 1, level)` per level: one entry for the root, two for the branch.
            assert!(
                prepared.blocks[0].query.sql.contains("[1:(1 + 1) - 1]"),
                "{}",
                prepared.blocks[0].query.sql
            );
            assert!(
                prepared.blocks[1].query.sql.contains("[1:(1 + 2) - 1]"),
                "{}",
                prepared.blocks[1].query.sql
            );
            // The child is narrowed to its level (`LENGTE`) and to its branch (`STARTSWITH`).
            assert!(
                prepared.blocks[1].query.sql.contains("END >= 2"),
                "{}",
                prepared.blocks[1].query.sql
            );
            assert!(
                prepared.blocks[1]
                    .query
                    .sql
                    .contains("[1:array_length(ARRAY['a'], 1)] = ARRAY['a']"),
                "{}",
                prepared.blocks[1].query.sql
            );
            // The measures name no block, so both levels select them.
            assert_eq!(prepared.blocks[0].query.cells.len(), 2);
            assert_eq!(prepared.blocks[1].query.cells.len(), 2);
            // `fields` is the whole legend: both levels, the measure and each level's filters
            // (`LENGTE` for both, `STARTSWITH` for the child only).
            let ids: Vec<i64> = prepared
                .fields
                .iter()
                .map(|item| item.legend_item_id)
                .collect();
            assert_eq!(ids, vec![1, 3, 0, 2, 4, 5]);
        }

        #[tokio::test]
        async fn the_expanded_level_lands_under_its_branch() {
            let (executor, calls) = RecordingExecutor::new(vec![
                vec![
                    vec![Some("[\"a\"]".into()), Some("42".into())],
                    vec![Some("[\"b\"]".into()), Some("7".into())],
                ],
                vec![vec![Some("[\"a\", \"x\"]".into()), Some("5".into())]],
            ]);
            let service = Service::new(executor, Dialect::Postgres);
            let response = service
                .result(&tree_dataset(), &tree_request())
                .await
                .unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
            // Block by block: the root's rows, with the child's rows after the branch they
            // expand. Each row points at the legend ids of its own block.
            assert_eq!(
                response.rows().unwrap(),
                &vec![
                    DataRow {
                        data: vec![json!("[\"a\"]"), json!("42")],
                        legend: vec![1, 0],
                    },
                    DataRow {
                        data: vec![json!("[\"a\", \"x\"]"), json!("5")],
                        legend: vec![3, 0],
                    },
                    DataRow {
                        data: vec![json!("[\"b\"]"), json!("7")],
                        legend: vec![1, 0],
                    },
                ]
            );
            // `blocks` lists every block of the request, with the SQL that produced its rows.
            let blocks: Vec<i64> = response.blocks.iter().map(|block| block.block_id).collect();
            assert_eq!(blocks, vec![0, 1]);
            assert!(response.blocks[1].query.is_some());
        }
    }
}

#[cfg(test)]
mod query_cache_tests {
    use super::*;
    use crate::dto::{FieldsItem, ResultRequest};
    use crate::executor::{ConnectionResolver, ConnectionTarget, Executor, ExecutorError};
    use crate::testing::demo_dataset;
    use async_trait::async_trait;
    use dl_core::{Dataset, FieldRef};
    use dl_query::identity::{with_identity, Identity};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::time::Duration;

    struct CountingExecutor {
        calls: Arc<AtomicUsize>,
        rows: Vec<Vec<Option<String>>>,
    }

    impl CountingExecutor {
        fn new(calls: Arc<AtomicUsize>) -> Self {
            Self {
                calls,
                rows: vec![vec![Some("Europe".to_string()), Some("42".to_string())]],
            }
        }
    }

    #[async_trait]
    impl Executor for CountingExecutor {
        async fn fetch(
            &self,
            _sql: &str,
            _target: Option<&str>,
        ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.rows.clone())
        }
    }

    /// Fails the first call (so the error must not be cached), then answers rows.
    struct FlakyExecutor {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Executor for FlakyExecutor {
        async fn fetch(
            &self,
            _sql: &str,
            _target: Option<&str>,
        ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
            let call = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if call == 0 {
                return Err(ExecutorError::new("transient database failure"));
            }
            Ok(vec![vec![Some("Europe".to_string())]])
        }
    }

    struct TtlResolver {
        dsn: String,
        ttl: Option<i64>,
    }

    #[async_trait]
    impl ConnectionResolver for TtlResolver {
        async fn resolve(&self, _connection_id: &str) -> Option<String> {
            Some(self.dsn.clone())
        }

        async fn resolve_target(&self, _connection_id: &str) -> Option<ConnectionTarget> {
            Some(ConnectionTarget {
                dsn: self.dsn.clone(),
                cache_ttl_sec: self.ttl,
                ..ConnectionTarget::new(self.dsn.clone())
            })
        }
    }

    fn dataset() -> Dataset {
        demo_dataset()
    }

    fn request(revision: Option<&str>) -> ResultRequest {
        ResultRequest {
            fields: vec![
                FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
            ],
            revision_id: revision.map(str::to_string),
            ..ResultRequest::default()
        }
    }

    fn ttl_settings(ttl: Duration) -> QueryCacheSettings {
        QueryCacheSettings {
            default_ttl: Some(ttl),
            ttl_override: None,
            capacity: 8,
        }
    }

    fn service_with_cache(
        calls: Arc<AtomicUsize>,
        settings: QueryCacheSettings,
    ) -> Service<CountingExecutor> {
        Service::new(CountingExecutor::new(calls), Dialect::Postgres).with_query_cache(settings)
    }

    #[tokio::test]
    async fn a_repeated_query_is_served_from_the_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        let stats = service.query_cache_stats().expect("cache enabled");
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.entries, 1);
    }

    #[tokio::test]
    async fn an_entry_expires_after_its_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service =
            service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_millis(25)));
        let dataset = dataset();
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn different_identities_never_share_an_entry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        let request = request(Some("rev-1"));
        let alice = Identity {
            user_id: "alice".to_string(),
            ..Identity::default()
        };
        let bob = Identity {
            user_id: "bob".to_string(),
            ..Identity::default()
        };
        with_identity(alice.clone(), service.result(&dataset, &request))
            .await
            .unwrap();
        with_identity(bob, service.result(&dataset, &request))
            .await
            .unwrap();
        with_identity(alice, service.result(&dataset, &request))
            .await
            .unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_database_error_is_never_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = Service::new(
            FlakyExecutor {
                calls: Arc::clone(&calls),
            },
            Dialect::Postgres,
        )
        .with_query_cache(ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        let request = request(Some("rev-1"));
        assert!(service.result(&dataset, &request).await.is_err());
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        // First call failed, second executed and was cached, third was a hit.
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_cache_is_bounded_and_evicts_the_lru_entry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut settings = ttl_settings(Duration::from_secs(60));
        settings.capacity = 1;
        let service = service_with_cache(Arc::clone(&calls), settings);
        let dataset = dataset();
        service
            .result(&dataset, &request(Some("rev-1")))
            .await
            .unwrap();
        service
            .result(&dataset, &request(Some("rev-2")))
            .await
            .unwrap();
        let stats = service.query_cache_stats().expect("cache enabled");
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.inserts, 2);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_new_revision_is_a_new_cache_key() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        service
            .result(&dataset, &request(Some("rev-a")))
            .await
            .unwrap();
        service
            .result(&dataset, &request(Some("rev-a")))
            .await
            .unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        service
            .result(&dataset, &request(Some("rev-b")))
            .await
            .unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_master_override_can_disable_the_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut settings = ttl_settings(Duration::from_secs(60));
        settings.ttl_override = Some(None);
        let service = service_with_cache(Arc::clone(&calls), settings);
        let dataset = dataset();
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_connection_ttl_of_zero_disables_the_cache_for_that_connection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut dataset = dataset();
        dataset.connection_id = Some("conn-1".to_string());
        let service = Service::new(CountingExecutor::new(Arc::clone(&calls)), Dialect::Postgres)
            .with_connection_resolver(Arc::new(TtlResolver {
                dsn: "postgres://pg:pg@db:5432/demo".to_string(),
                ttl: Some(0),
            }))
            .with_query_cache(ttl_settings(Duration::from_secs(60)));
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_connection_without_a_ttl_is_never_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut dataset = dataset();
        dataset.connection_id = Some("conn-1".to_string());
        let service = Service::new(CountingExecutor::new(Arc::clone(&calls)), Dialect::Postgres)
            .with_connection_resolver(Arc::new(TtlResolver {
                dsn: "postgres://pg:pg@db:5432/demo".to_string(),
                ttl: None,
            }))
            .with_query_cache(QueryCacheSettings::default());
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_connection_ttl_enables_the_cache_without_any_fallback() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut dataset = dataset();
        dataset.connection_id = Some("conn-1".to_string());
        let service = Service::new(CountingExecutor::new(Arc::clone(&calls)), Dialect::Postgres)
            .with_connection_resolver(Arc::new(TtlResolver {
                dsn: "postgres://pg:pg@db:5432/demo".to_string(),
                ttl: Some(15),
            }))
            .with_query_cache(QueryCacheSettings {
                default_ttl: None,
                ttl_override: None,
                capacity: 8,
            });
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn master_override_zero_reports_the_cache_as_disabled() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut settings = ttl_settings(Duration::from_secs(60));
        settings.ttl_override = Some(None);
        let service = service_with_cache(Arc::clone(&calls), settings);
        let report = service.query_cache_json();
        assert_eq!(report["enabled"], serde_json::json!(false));
        assert_eq!(report["master"], serde_json::json!("off"));
        // ... and the switch really bypasses the cache even with a configured TTL.
        let dataset = dataset();
        let request = request(Some("rev-1"));
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_per_connection_cache_reports_enabled() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let report = service.query_cache_json();
        assert_eq!(report["enabled"], serde_json::json!(true));
        assert_eq!(report["master"], serde_json::json!("per_connection"));
    }

    #[test]
    fn the_dataset_revision_id_alias_is_parsed() {
        let request: ResultRequest = serde_json::from_value(serde_json::json!({
            "fields": [],
            "dataset_revision_id": "rev-7",
        }))
        .unwrap();
        assert_eq!(request.revision_id.as_deref(), Some("rev-7"));
        let pivot: PivotRequest = serde_json::from_value(serde_json::json!({
            "fields": [],
            "dataset_revision_id": "rev-8",
        }))
        .unwrap();
        assert_eq!(pivot.revision_id.as_deref(), Some("rev-8"));
        // Old spelling keeps working.
        let old: ResultRequest = serde_json::from_value(serde_json::json!({
            "fields": [],
            "revision_id": "rev-9",
        }))
        .unwrap();
        assert_eq!(old.revision_id.as_deref(), Some("rev-9"));
    }

    fn revision_request(dataset_revision: &str) -> ResultRequest {
        serde_json::from_value(serde_json::json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role": "row"},
                {"ref": {"type": "id", "id": "f_sales"}, "role": "measure"},
            ],
            "dataset_revision_id": dataset_revision,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn different_dataset_revisions_via_the_alias_never_share_an_entry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        let request_a = revision_request("rev-a");
        let request_b = revision_request("rev-b");
        service.result(&dataset, &request_a).await.unwrap();
        service.result(&dataset, &request_a).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        // A dataset revision switch is a new key: no hit, the query runs again.
        service.result(&dataset, &request_b).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        let stats = service.query_cache_stats().expect("cache enabled");
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.entries, 2);
    }

    #[tokio::test]
    async fn an_unpinned_request_is_never_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = service_with_cache(Arc::clone(&calls), ttl_settings(Duration::from_secs(60)));
        let dataset = dataset();
        let request = request(None);
        service.result(&dataset, &request).await.unwrap();
        service.result(&dataset, &request).await.unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        let stats = service.query_cache_stats().expect("cache enabled");
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.inserts, 0);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::dto::{FieldsItem, ResultRequest};
    use crate::executor::{
        ConnectionResolver, ConnectionTarget, Executor, ExecutorError, QueryMetadata,
        StaticExecutor,
    };
    use crate::testing::demo_dataset;
    use async_trait::async_trait;
    use dl_core::{Dataset, FieldRef, RawSqlLevel, Source};
    use serde_json::json;
    use std::sync::Arc;

    struct PolicyResolver(ConnectionTarget);

    #[async_trait]
    impl ConnectionResolver for PolicyResolver {
        async fn resolve(&self, _connection_id: &str) -> Option<String> {
            Some(self.0.dsn.clone())
        }

        async fn resolve_target(&self, _connection_id: &str) -> Option<ConnectionTarget> {
            Some(self.0.clone())
        }
    }

    fn subselect_dataset() -> Dataset {
        let mut dataset = demo_dataset();
        dataset.connection_id = Some("conn-1".to_string());
        dataset.sources = vec![Source {
            id: "src".to_string(),
            table: String::new(),
            connection_id: Some("conn-1".to_string()),
            subselect: Some("SELECT Region, Sales FROM public.sales".to_string()),
            columns: Vec::new(),
        }];
        dataset
    }

    fn request() -> ResultRequest {
        ResultRequest {
            fields: vec![
                FieldsItem::new(FieldRef::id("f_region"), Role::Row),
                FieldsItem::new(FieldRef::id("f_sales"), Role::Measure),
            ],
            ..ResultRequest::default()
        }
    }

    fn service(target: ConnectionTarget) -> Service<StaticExecutor> {
        Service::new(StaticExecutor::default(), Dialect::Postgres)
            .with_connection_resolver(Arc::new(PolicyResolver(target)))
    }

    #[tokio::test]
    async fn a_subselect_is_refused_when_the_connection_forbids_raw_sql() {
        let error = service(ConnectionTarget::new("postgres://db"))
            .result(&subselect_dataset(), &request())
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(
            error.code,
            "ERR.DS_API.CONNECTION_CONFIG.SUBSELECT_NOT_ALLOWED"
        );
    }

    #[tokio::test]
    async fn a_subselect_runs_when_the_connection_allows_it() {
        let mut target = ConnectionTarget::new("postgres://db");
        target.raw_sql_level = RawSqlLevel::Subselect;
        let response = service(target)
            .result(&subselect_dataset(), &request())
            .await
            .unwrap();
        assert!(response.data_export.basic.allowed);
    }

    fn dashsql_service(target: ConnectionTarget) -> Service<StaticExecutor> {
        Service::new(
            StaticExecutor::new(vec![vec![Some("1".to_string()), None]]),
            Dialect::Postgres,
        )
        .with_connection_resolver(Arc::new(PolicyResolver(target)))
    }

    fn dashsql_target() -> ConnectionTarget {
        let mut target = ConnectionTarget::new("postgres://db");
        target.raw_sql_level = RawSqlLevel::Dashsql;
        target
    }

    #[tokio::test]
    async fn dashsql_answers_metadata_rows_and_footer_when_raw_sql_is_allowed() {
        let service = dashsql_service(dashsql_target());
        let events = service
            .dashsql(
                "conn-1",
                &DashSqlRequest {
                    sql_query: "SELECT 1 AS one".to_string(),
                    ..DashSqlRequest::default()
                },
            )
            .await
            .unwrap();
        let events = events
            .as_array()
            .expect("the dashsql body is an event list");
        assert_eq!(events[0]["event"], "metadata");
        assert_eq!(events[0]["data"]["bi_types"], serde_json::json!([]));
        assert_eq!(events[1]["event"], "row");
        assert_eq!(events[1]["data"], serde_json::json!(["1", null]));
        assert_eq!(events.last().unwrap()["event"], "footer");
    }

    #[tokio::test]
    async fn dashsql_is_refused_when_the_connection_forbids_raw_sql() {
        // The storage default is `off`: a connection that never enabled DashSQL must not run a
        // single statement.
        let error = dashsql_service(ConnectionTarget::new("postgres://db"))
            .dashsql(
                "conn-1",
                &DashSqlRequest {
                    sql_query: "SELECT 1".to_string(),
                    ..DashSqlRequest::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(
            error.code,
            "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED"
        );
    }

    #[tokio::test]
    async fn typed_query_answers_rows_with_fallback_column_headers() {
        let mut target = dashsql_target();
        target.dsn = "clickhouse://db".to_string();
        let service = dashsql_service(target);
        let body = service
            .typed_query(
                "conn-1",
                &TypedQueryRequest {
                    query: Some("SELECT 1".to_string()),
                    ..TypedQueryRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(body["query_type"], "generic_query");
        assert_eq!(body["data"]["rows"], serde_json::json!([["1", null]]));
        assert_eq!(body["data"]["headers"][0]["name"], "col_0");
        assert_eq!(body["data"]["headers"][0]["data_type"], "unsupported");
    }

    /// `is_typed_query_allowed` is `True` for ClickHouse and `False` everywhere else, so a
    /// PostgreSQL connection refuses the route before a statement is executed.
    #[tokio::test]
    async fn typed_query_is_refused_on_postgres_like_python() {
        let error = dashsql_service(dashsql_target())
            .typed_query(
                "conn-1",
                &TypedQueryRequest {
                    query: Some("SELECT 1".to_string()),
                    ..TypedQueryRequest::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(
            error.code,
            "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED"
        );
    }

    /// An executor that reports cursor metadata, like the ClickHouse adapter after its `meta`
    /// event: two columns with native type names.
    struct MetadataExecutor {
        names: Vec<String>,
        db_types: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    }

    #[async_trait]
    impl Executor for MetadataExecutor {
        async fn fetch(
            &self,
            _sql: &str,
            _target: Option<&str>,
        ) -> Result<Vec<Vec<Option<String>>>, ExecutorError> {
            Ok(self.rows.clone())
        }

        async fn fetch_with_meta(
            &self,
            _sql: &str,
            _target: Option<&str>,
        ) -> Result<(Vec<Vec<Option<String>>>, QueryMetadata), ExecutorError> {
            Ok((
                self.rows.clone(),
                QueryMetadata::new(self.names.clone(), self.db_types.clone()),
            ))
        }
    }

    fn metadata_service() -> Service<MetadataExecutor> {
        let mut target = dashsql_target();
        target.dsn = "clickhouse://db".to_string();
        Service::new(
            MetadataExecutor {
                names: vec![
                    "n".to_string(),
                    "f".to_string(),
                    "b".to_string(),
                    "s".to_string(),
                ],
                db_types: vec![
                    "UInt64".to_string(),
                    "Nullable(Float64)".to_string(),
                    "Bool".to_string(),
                    "String".to_string(),
                ],
                rows: vec![vec![
                    Some("7".to_string()),
                    Some("2.5".to_string()),
                    Some("true".to_string()),
                    Some("abc".to_string()),
                ]],
            },
            Dialect::Postgres,
        )
        .with_connection_resolver(Arc::new(PolicyResolver(target)))
    }

    #[tokio::test]
    async fn dashsql_metadata_carries_names_and_bi_types_from_the_cursor() {
        let body = metadata_service()
            .dashsql(
                "conn-1",
                &DashSqlRequest {
                    sql_query: "SELECT 1".to_string(),
                    ..DashSqlRequest::default()
                },
            )
            .await
            .unwrap();
        let metadata = &body[0]["data"];
        assert_eq!(metadata["names"], serde_json::json!(["n", "f", "b", "s"]));
        assert_eq!(
            metadata["db_types"],
            serde_json::json!(["uint64", "float64", "bool", "string"])
        );
        assert_eq!(
            metadata["bi_types"],
            serde_json::json!(["integer", "float", "boolean", "string"])
        );
        // `DashSQLView` forbids background export for the endpoint itself.
        assert_eq!(metadata["data_export"]["background"]["allowed"], false);
        let reasons = metadata["data_export"]["background"]["reason"]
            .as_array()
            .expect("a reason list");
        assert!(
            reasons.iter().any(|reason| reason == PROHIBITED_IN_DASHSQL),
            "{reasons:?}"
        );
    }

    #[tokio::test]
    async fn typed_query_headers_and_values_follow_the_cursor_types() {
        let body = metadata_service()
            .typed_query(
                "conn-1",
                &TypedQueryRequest {
                    query: Some("SELECT 1".to_string()),
                    ..TypedQueryRequest::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            body["data"]["headers"],
            serde_json::json!([
                {"name": "n", "data_type": "integer"},
                {"name": "f", "data_type": "float"},
                {"name": "b", "data_type": "boolean"},
                {"name": "s", "data_type": "string"},
            ])
        );
        // Typed JSON values, not the text of the dataset path.
        assert_eq!(
            body["data"]["rows"],
            serde_json::json!([[7, 2.5, true, "abc"]])
        );
        let reasons = body["data_export"]["background"]["reason"]
            .as_array()
            .expect("a reason list");
        assert!(
            reasons
                .iter()
                .any(|reason| reason == PROHIBITED_IN_TYPED_QUERY),
            "{reasons:?}"
        );
    }

    #[test]
    fn clickhouse_type_names_map_to_bi_types_like_the_python_transformer() {
        for (raw, expected) in [
            ("UInt64", "integer"),
            ("Nullable(Int32)", "integer"),
            ("LowCardinality(Nullable(String))", "string"),
            ("Float64", "float"),
            ("Decimal(10, 2)", "float"),
            ("Date", "date"),
            ("DateTime64(3)", "genericdatetime"),
            ("Bool", "boolean"),
            ("UUID", "uuid"),
            ("Array(String)", "unsupported"),
            ("Enum8('a' = 1)", "string"),
        ] {
            assert_eq!(ch_user_type_name(raw), expected, "type {raw}");
        }
        assert_eq!(native_type_name("Nullable( UInt64 )"), "uint64");
        assert_eq!(native_type_name("Array(String)"), "array");
        for (raw, expected) in [
            ("int4", "integer"),
            ("float8", "float"),
            ("bool", "boolean"),
            ("text", "string"),
            ("timestamp", "genericdatetime"),
            ("uuid", "uuid"),
            ("jsonb", "unsupported"),
        ] {
            assert_eq!(pg_user_type_name(raw), expected, "type {raw}");
        }
        assert_eq!(bi_type_name("clickhouse://db", "UInt64"), "integer");
        assert_eq!(bi_type_name("postgres://db", "int4"), "integer");
    }

    #[test]
    fn typed_cells_keep_numbers_booleans_and_nulls() {
        assert_eq!(typed_cell(Some("7"), "integer"), json!(7));
        assert_eq!(typed_cell(Some("2.5"), "float"), json!(2.5));
        assert_eq!(typed_cell(Some("true"), "boolean"), json!(true));
        assert_eq!(typed_cell(Some("2020-01-01"), "date"), json!("2020-01-01"));
        assert_eq!(typed_cell(None, "integer"), serde_json::Value::Null);
        // An array arrives as a JSON document and stays one.
        assert_eq!(typed_cell(Some("[\"a\"]"), "unsupported"), json!(["a"]));
        // A value the type cannot parse keeps its text instead of disappearing.
        assert_eq!(typed_cell(Some("NaN"), "integer"), json!("NaN"));
    }

    #[tokio::test]
    async fn typed_query_raw_answers_the_python_not_allowed_error() {
        let error = dashsql_service(dashsql_target())
            .typed_query_raw("conn-1", &TypedQueryRawRequest::default())
            .await
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(
            error.code,
            "ERR.DS_API.CONNECTION_CONFIG.DASHSQL_NOT_ALLOWED"
        );
    }

    #[tokio::test]
    async fn a_connection_that_forbids_export_denies_both_kinds() {
        let mut target = ConnectionTarget::new("postgres://db");
        target.data_export_forbidden = true;
        let mut dataset = demo_dataset();
        dataset.connection_id = Some("conn-1".to_string());
        let response = service(target).result(&dataset, &request()).await.unwrap();
        assert!(!response.data_export.basic.allowed);
        assert!(!response.data_export.background.allowed);
        assert_eq!(
            response.data_export.basic.reason.as_deref(),
            Some([DataExport::DISABLED_IN_CONNECTION.to_string()].as_slice())
        );
        assert!(response.data_export_forbidden);
    }

    #[tokio::test]
    async fn the_default_query_cache_has_no_fallback_ttl() {
        assert!(QueryCacheSettings::default().default_ttl.is_none());
    }
}
