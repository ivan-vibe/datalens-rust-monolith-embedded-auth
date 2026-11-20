//! Shared application state and the `ctx.get('info')`-equivalent request info.

use std::sync::Arc;

use axum::http::HeaderMap;

use crate::config::Config;
use crate::db::Db;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
}

pub const DL_COMPONENT_HEADER: &str = "x-dl-component";

/// `resolvePrivatePermissions` from `src/components/private-permissions.ts`.
#[derive(Debug, Clone, Default)]
pub struct PrivatePermissions {
    pub owned_scopes: &'static [&'static str],
}

impl PrivatePermissions {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let component = headers
            .get(DL_COMPONENT_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();

        let owned_scopes: &'static [&'static str] = match component {
            "backend" => &["config", "connection", "dataset"],
            "ui" => &["dash", "widget", "report"],
            _ => &[],
        };

        Self { owned_scopes }
    }

    /// `filterUnversionedData`.
    pub fn filter_unversioned_data(
        &self,
        scope: &str,
        unversioned_data: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        if self.owned_scopes.contains(&scope) {
            unversioned_data
        } else {
            None
        }
    }
}

/// `ctx.get('info').user`.
#[derive(Debug, Clone, Default)]
pub struct UserInfo {
    /// Kept for the future write endpoints (`ctx.get('info').user.userId`).
    #[allow(dead_code)]
    pub user_id: Option<String>,
    pub login: Option<String>,
}
