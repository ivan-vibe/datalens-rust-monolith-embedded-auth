//! `us` (United Storage) client for the Rust data-api.
//!
//! The Python data-api never serves a dataset from its own memory: it reads the entry from
//! `us` (`dl_core.us_manager`), applies `updates` and compiles the query. This crate is that
//! first step — the `GET /private/entries/{id}` call, the dataset JSON decoder and a TTL
//! cache — so `datalens-server` can serve `/api/data/v2/datasets/{ds_id}/...` for real
//! dataset ids instead of a local store.
//!
//! Everything here is transport-level: no `dl_api` types are involved, which keeps the crate
//! usable by a future control-api port as well.

pub mod cache;
pub mod client;
pub mod connection;
pub mod dataset;
pub mod entry;
pub mod error;
pub mod fernet;
pub mod testing;

pub use cache::UsDatasetCache;
pub use client::{
    direct_dataset, install_dataset_direct, install_direct, install_local, install_value_direct,
    DatasetResolver, DirectResolver, LocalRequest, LocalResponse, UsAuth, UsClient, ValueResolver,
};
pub use connection::{ConnectionSecrets, ConnectorKind, UsConnection, UsConnectionCache};
pub use dataset::to_dataset;
pub use entry::{UsConnectionData, UsDatasetData, UsEntry};
pub use error::UsError;
pub use fernet::FernetKey;

tokio::task_local! {
    pub static SNAPSHOT: String;
}

pub fn current_snapshot() -> Option<String> {
    SNAPSHOT.try_with(|value| value.clone()).ok()
}
