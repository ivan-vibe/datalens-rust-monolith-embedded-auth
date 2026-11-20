pub mod collections;
pub mod compat;
pub mod docs;
pub mod entries;
pub mod entries_copy;
pub mod entries_read;
pub mod entries_write;
pub mod favorites;
pub mod health;
pub mod locks;
pub mod msgpack;
pub mod not_found;
pub mod options;
pub mod palettes;
pub mod states;
pub mod tenants;
pub mod workbooks;

use axum::routing::MethodRouter;
use axum::Router;

use crate::state::AppState;

/// Registers a route both with and without a trailing slash.
///
/// express runs with `strict routing` disabled, so `GET /v1/entries/<id>/`
/// reaches the same handler as `GET /v1/entries/<id>`; axum matches exactly, so
/// both spellings have to be registered explicitly.
pub fn dual_router(routes: Vec<(&'static str, MethodRouter<AppState>)>) -> Router<AppState> {
    let mut router = Router::new();

    for (path, method) in routes {
        let mut both = path.to_owned();
        both.push('/');
        router = router.route(path, method.clone()).route(&both, method);
    }

    router
}
