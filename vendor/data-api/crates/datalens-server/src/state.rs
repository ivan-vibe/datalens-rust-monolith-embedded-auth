//! Server state: the compiled service, the local dataset store and `us`.

use std::collections::HashMap;
use std::sync::Arc;

use dl_api::{ApiError, Service};
use dl_core::Dataset;
use dl_query::Dialect;
use dl_us::{to_dataset, UsDatasetCache, UsError};

use crate::auth::{current_bearer, NativeAuth};
use crate::executor::AnyExecutor;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<Service<AnyExecutor>>,
    pub store: Arc<DatasetStore>,
    /// `None` when the service runs without `us` (dry runs, tests, `DL_DATASETS`).
    pub us: Option<Arc<UsDatasetCache>>,
    /// `Some` only for `AUTH__TYPE=NATIVE`; the middleware then requires a JWT.
    pub auth: Option<NativeAuth>,
}

impl AppState {
    pub fn new(service: Arc<Service<AnyExecutor>>, store: Arc<DatasetStore>) -> Self {
        Self {
            service,
            store,
            us: None,
            auth: None,
        }
    }

    /// Require a valid JWT for every route except `/ping` and `/ping_ready`.
    pub fn with_auth(mut self, auth: NativeAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Resolve datasets through `us` as well as the local store.
    pub fn with_us(mut self, us: Arc<UsDatasetCache>) -> Self {
        self.us = Some(us);
        self
    }

    /// Swap the service: the `us` path builds one that resolves connections into DSNs.
    pub fn with_service(mut self, service: Arc<Service<AnyExecutor>>) -> Self {
        self.service = service;
        self
    }

    pub fn dialect(&self) -> Dialect {
        self.service.dialect()
    }

    /// The dataset behind `ds_id`, or the error `data-api` returns for a missing one.
    ///
    /// The local store wins over `us`: it holds the demo ids and the `DL_DATASETS` fixture,
    /// which is what the smoke tests and benchmarks use. Both answer an `Arc`, so a dashboard
    /// that opens twelve charts on one dataset parses and copies it once, not twelve times.
    pub async fn dataset_with_revision(
        &self,
        ds_id: &str,
    ) -> Result<(Arc<Dataset>, Option<String>), ApiError> {
        let started = std::time::Instant::now();
        if let Some(dataset) = self.store.get(ds_id) {
            dl_api::trace::record_work("dataset", started.elapsed());
            return Ok((dataset, None));
        }
        let result = match self.us_dataset(ds_id).await {
            Ok(dataset) => Ok(dataset),
            Err(error) => Err(error),
        };
        if self.us.is_some() {
            dl_api::trace::record_wait("dataset", started.elapsed());
        } else {
            dl_api::trace::record_work("dataset", started.elapsed());
        }
        result
    }

    pub async fn dataset_rev(
        &self,
        ds_id: &str,
        revision: Option<&str>,
    ) -> Result<Arc<Dataset>, ApiError> {
        let started = std::time::Instant::now();
        if let Some(dataset) = self.store.get(ds_id) {
            dl_api::trace::record_work("dataset", started.elapsed());
            return Ok(dataset);
        }
        // Monolith chart path: the request-scoped bundle already decoded this dataset. Serve
        // that `Arc<Dataset>` when the pinned revision agrees; a mismatch falls through to the
        // per-user US read below, which validates the pin.
        if let Some((dataset, direct_revision)) = dl_us::direct_dataset(ds_id) {
            if revision.map_or(true, |wanted| direct_revision.as_deref() == Some(wanted)) {
                dl_api::trace::record_work("dataset", started.elapsed());
                return Ok(dataset);
            }
        }
        // With a caller identity the read is per-user even when a revision is pinned: the
        // shared cache is keyed by entry/revision only, so it must not bypass the US ACL.
        let per_user =
            crate::auth::current_bearer().is_some() || dl_query::identity::current().is_some();
        let result = match revision {
            Some(revision) => {
                match self.us.as_ref() {
                    Some(us) => {
                        // A pinned revision must also be read per-user when a bearer is scoped:
                        // otherwise the pin would be dropped and a stale/other revision answered.
                        let pinned = match crate::auth::current_bearer() {
                            Some(token) => us.client().get_entry_as(ds_id, &token).await,
                            None if per_user => us.client().get_entry(ds_id).await,
                            None => {
                                return us
                                    .dataset_pinned(ds_id, Some(revision))
                                    .await
                                    .map_err(|error| api_error(ds_id, error));
                            }
                        };
                        pinned
                        .map_err(|error| api_error(ds_id, error))
                        .and_then(|entry| {
                            if entry.rev_id.as_deref() != Some(revision) {
                                return Err(ApiError::new(
                                    400,
                                    "ERR.DS_API.US.NOT_EXIST_REVISION",
                                    format!("revision {revision} does not exist for dataset {ds_id}"),
                                ));
                            }
                            to_dataset(
                                ds_id,
                                &entry
                                    .dataset_data()
                                    .map_err(|error| api_error(ds_id, error))?,
                            )
                            .map(Arc::new)
                            .map_err(|error| api_error(ds_id, error))
                        })
                    }
                    None => Err(ApiError::not_found(format!(
                        "dataset {ds_id} does not exist"
                    ))),
                }
            }
            None => self.us_dataset(ds_id).await.map(|(dataset, _)| dataset),
        };
        if self.us.is_some() {
            dl_api::trace::record_wait("dataset", started.elapsed());
        } else {
            dl_api::trace::record_work("dataset", started.elapsed());
        }
        result
    }

    pub async fn dataset(&self, ds_id: &str) -> Result<Arc<Dataset>, ApiError> {
        let started = std::time::Instant::now();
        if let Some(dataset) = self.store.get(ds_id) {
            dl_api::trace::record_work("dataset", started.elapsed());
            return Ok(dataset);
        }
        let result = self.us_dataset(ds_id).await.map(|(dataset, _)| dataset);
        if self.us.is_some() {
            dl_api::trace::record_wait("dataset", started.elapsed());
        } else {
            dl_api::trace::record_work("dataset", started.elapsed());
        }
        result
    }

    /// Read a dataset from `us` on behalf of the caller.
    ///
    /// The shared `UsDatasetCache` is keyed by entry id only, so it must not serve a
    /// per-user read: a user whose ACL changed, or another tenant, would get the previous
    /// answer. The HTTP path forwards the caller's bearer (`get_entry_as`); the in-process
    /// chart path (`data_direct`) scopes the validated identity but not yet the raw token, so
    /// it falls back to the service credentials without the shared cache and logs that gap.
    async fn us_dataset(&self, ds_id: &str) -> Result<(Arc<Dataset>, Option<String>), ApiError> {
        // Monolith chart path: the request-scoped US bundle already decoded this dataset.
        if let Some((dataset, revision)) = dl_us::direct_dataset(ds_id) {
            return Ok((dataset, revision));
        }
        let Some(us) = self.us.as_ref() else {
            return Err(ApiError::not_found(format!(
                "dataset {ds_id} does not exist"
            )));
        };
        let bearer = current_bearer();
        if let Some(token) = bearer.as_deref() {
            let entry = us
                .client()
                .get_entry_as(ds_id, token)
                .await
                .map_err(|error| api_error(ds_id, error))?;
            let dataset = to_dataset(
                ds_id,
                &entry
                    .dataset_data()
                    .map_err(|error| api_error(ds_id, error))?,
            )
            .map_err(|error| api_error(ds_id, error))?;
            return Ok((Arc::new(dataset), entry.rev_id.clone()));
        }
        // No token on the request: the chart path validated the identity but does not expose
        // the raw bearer to this crate. Refuse to answer when an auth backend is configured —
        // the middleware already guarantees a token there. Without an auth backend (`AUTH__TYPE`
        // unset) there is no user identity to isolate by, which is an explicitly trusted
        // deployment, and the shared cache is used as before.
        let identity = dl_query::identity::current();
        if self.auth.is_some() && identity.is_none() {
            return Err(ApiError::new(
                401,
                "ERR.DS_API.REQUEST.AUTH",
                "User access token header is missing",
            ));
        }
        if identity.is_some() {
            // TODO(g02): `data_direct` must scope the bearer token too; until then this read
            // uses the service credentials and bypasses the global dataset cache so one user's
            // answer can never be served to another.
            let entry = us
                .client()
                .get_entry(ds_id)
                .await
                .map_err(|error| api_error(ds_id, error))?;
            let dataset = to_dataset(
                ds_id,
                &entry
                    .dataset_data()
                    .map_err(|error| api_error(ds_id, error))?,
            )
            .map_err(|error| api_error(ds_id, error))?;
            return Ok((Arc::new(dataset), entry.rev_id.clone()));
        }
        us.dataset_with_revision(ds_id, None)
            .await
            .map_err(|error| api_error(ds_id, error))
    }
}

/// `dl_api_lib` returns the `us` status and code as they are, so the same error is answered
/// to the caller (a `us` 404 stays a 404).
fn api_error(ds_id: &str, error: UsError) -> ApiError {
    tracing::warn!(dataset = ds_id, "us lookup failed: {error}");
    ApiError::new(error.status(), error.code(), error.to_string())
}

/// Datasets the server can answer for. The Python service fetches them from `us`; here they
/// come from a JSON file (`DL_DATASETS`) or from the built-in demo dataset.
///
/// The values are `Arc<Dataset>`: a request never owns the dataset it runs on, so the map can
/// hand the same one to every concurrent chart of a dashboard.
#[derive(Debug, Default, Clone)]
pub struct DatasetStore {
    datasets: HashMap<String, Arc<Dataset>>,
}

impl DatasetStore {
    pub fn new(datasets: HashMap<String, Dataset>) -> Self {
        Self {
            datasets: datasets
                .into_iter()
                .map(|(ds_id, dataset)| (ds_id, Arc::new(dataset)))
                .collect(),
        }
    }

    /// The demo dataset under the ids the benchmarks use, so `/result` works out of the box.
    pub fn with_defaults() -> Self {
        let mut datasets = HashMap::new();
        let demo = Arc::new(dl_api::testing::demo_dataset());
        datasets.insert("demo".to_string(), demo.clone());
        datasets.insert("a8b1e0dc-0000-0000-0000-000000000001".to_string(), demo);
        Self { datasets }
    }

    pub fn from_json(raw: &str) -> Result<Self, serde_json::Error> {
        let datasets: HashMap<String, Dataset> = serde_json::from_str(raw)?;
        Ok(Self::new(datasets))
    }

    pub fn insert(&mut self, ds_id: impl Into<String>, dataset: Dataset) {
        self.insert_arc(ds_id, Arc::new(dataset));
    }

    /// Insert a dataset that is already shared (a store built from another one, a `us` cache).
    pub fn insert_arc(&mut self, ds_id: impl Into<String>, dataset: Arc<Dataset>) {
        self.datasets.insert(ds_id.into(), dataset);
    }

    pub fn get(&self, ds_id: &str) -> Option<Arc<Dataset>> {
        self.datasets.get(ds_id).cloned()
    }

    pub fn ids(&self) -> Vec<&str> {
        self.datasets.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_serve_the_demo_dataset_and_unknown_ids_are_missing() {
        let store = DatasetStore::with_defaults();
        assert!(store.get("demo").is_some());
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn a_store_round_trips_through_json() {
        let mut store = DatasetStore::default();
        store.insert("ds", dl_api::testing::demo_dataset());
        let stored = store.get("ds").unwrap();
        let json = serde_json::json!({ "ds": &*stored }).to_string();
        let parsed = DatasetStore::from_json(&json).unwrap();
        assert_eq!(parsed.get("ds"), store.get("ds"));
    }
}
