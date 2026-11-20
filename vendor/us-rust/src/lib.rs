//! `us-rs` — a drop-in replacement binary for the DataLens `us` Node service.
//!
//! The library part is exposed so integration tests, benchmarks and the
//! `idtool` example can reuse the exact same code paths as the server.

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod id;
pub mod logging;
pub mod metrics;
pub mod migrations;
pub mod permissions;
pub mod schema;
pub mod state;
