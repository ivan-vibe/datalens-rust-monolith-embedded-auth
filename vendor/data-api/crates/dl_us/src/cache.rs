//! In-process cache of datasets loaded from `us`.
//!
//! The Python service reads a dataset on every request (with a US-level cache in front);
//! the data path only needs "entry id -> dataset" for a few seconds, so a TTL map is
//! enough. It also keeps a burst of `/result` calls for one dataset from hammering `us`.
//!
//! Unlike `dl_cache_engine` this cache holds *metadata*, never query results: results are
//! data, not configuration, and are not cached yet (see `PORTING.md`, milestone 6).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dl_core::Dataset;
use tokio::sync::RwLock;

use crate::client::UsClient;
use crate::dataset::to_dataset;
use crate::error::UsError;

/// Datasets cached by default.
const DEFAULT_CAPACITY: usize = 1024;

struct CachedDataset {
    /// `Arc`, not `Dataset`: a hit hands the same dataset to every request instead of deep
    /// copying the fields and sources of it (which is what a chart grid would pay 12 times).
    dataset: Arc<Dataset>,
    revision: Option<String>,
    loaded_at: Instant,
}

/// A `us` client plus a TTL cache of the datasets it returns.
pub struct UsDatasetCache {
    client: UsClient,
    ttl: Duration,
    capacity: usize,
    entries: RwLock<HashMap<String, CachedDataset>>,
}

impl UsDatasetCache {
    pub fn new(client: UsClient, ttl: Duration) -> Self {
        Self {
            client,
            ttl,
            capacity: DEFAULT_CAPACITY,
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn client(&self) -> &UsClient {
        &self.client
    }

    /// The dataset for `entry_id`, from the cache or from `us`.
    ///
    /// With an explicit revision the cached value is reused only when its `revId` is exactly the
    /// requested one. Without a revision the cache is bypassed: this keeps metadata fresh while
    /// still allowing a safe cache for revision-pinned requests.
    pub async fn dataset(&self, entry_id: &str) -> Result<Arc<Dataset>, UsError> {
        self.dataset_pinned(entry_id, None).await
    }

    pub async fn dataset_pinned(
        &self,
        entry_id: &str,
        revision: Option<&str>,
    ) -> Result<Arc<Dataset>, UsError> {
        Ok(self.dataset_with_revision(entry_id, revision).await?.0)
    }

    pub async fn dataset_with_revision(
        &self,
        entry_id: &str,
        revision: Option<&str>,
    ) -> Result<(Arc<Dataset>, Option<String>), UsError> {
        if let Some(revision) = revision {
            if let Some(dataset) = self.cached_revision(entry_id, revision).await {
                return Ok((dataset, Some(revision.to_string())));
            }
        }
        let entry = self.client.get_entry(entry_id).await?;
        let dataset = Arc::new(to_dataset(entry_id, &entry.dataset_data()?)?);
        let revision = entry.rev_id.clone();
        self.store(entry_id, dataset.clone(), revision.clone()).await;
        Ok((dataset, revision))
    }

    async fn cached_revision(&self, entry_id: &str, revision: &str) -> Option<Arc<Dataset>> {
        let guard = self.entries.read().await;
        let cached = guard.get(entry_id)?;
        (cached.revision.as_deref() == Some(revision)).then(|| cached.dataset.clone())
    }

    /// Forget one entry (used when a request says the dataset changed).
    pub async fn invalidate(&self, entry_id: &str) {
        self.entries.write().await.remove(entry_id);
    }

    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn cached(&self, entry_id: &str) -> Option<Arc<Dataset>> {
        let guard = self.entries.read().await;
        let cached = guard.get(entry_id)?;
        (cached.loaded_at.elapsed() < self.ttl).then(|| cached.dataset.clone())
    }

    async fn store(&self, entry_id: &str, dataset: Arc<Dataset>, revision: Option<String>) {
        let mut guard = self.entries.write().await;
        if guard.len() >= self.capacity {
            let ttl = self.ttl;
            guard.retain(|_, cached| cached.loaded_at.elapsed() < ttl);
            if guard.len() >= self.capacity {
                if let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, cached)| cached.loaded_at)
                    .map(|(key, _)| key.clone())
                {
                    guard.remove(&oldest);
                }
            }
        }
        guard.insert(
            entry_id.to_string(),
            CachedDataset {
                dataset,
                revision,
                loaded_at: Instant::now(),
            },
        );
    }
}

impl std::fmt::Debug for UsDatasetCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsDatasetCache")
            .field("base_url", &self.client.base_url())
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockUs;

    fn cache_for(us: &MockUs, ttl: Duration) -> UsDatasetCache {
        let client = UsClient::new(us.base_url()).unwrap();
        UsDatasetCache::new(client, ttl)
    }

    #[tokio::test]
    async fn a_dataset_is_fetched_once_within_the_ttl() {
        let us = MockUs::with_dataset().await;
        let cache = cache_for(&us, Duration::from_secs(60));
        let first = cache.dataset("dataset-1").await.unwrap();
        let second = cache.dataset("dataset-1").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(us.requests().len(), 1);
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn invalidate_forces_a_refetch() {
        let us = MockUs::with_dataset().await;
        let cache = cache_for(&us, Duration::from_secs(60));
        cache.dataset("dataset-1").await.unwrap();
        cache.invalidate("dataset-1").await;
        assert!(cache.is_empty().await);
        cache.dataset("dataset-1").await.unwrap();
        assert_eq!(us.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_zero_ttl_never_hits_the_cache() {
        let us = MockUs::with_dataset().await;
        let cache = cache_for(&us, Duration::ZERO);
        cache.dataset("dataset-1").await.unwrap();
        cache.dataset("dataset-1").await.unwrap();
        assert_eq!(us.requests().len(), 2);
    }

    #[tokio::test]
    async fn an_unknown_entry_is_not_cached() {
        let us = MockUs::with_dataset().await;
        let cache = cache_for(&us, Duration::from_secs(60));
        assert!(cache.dataset("missing").await.is_err());
        assert!(cache.is_empty().await);
    }
}
