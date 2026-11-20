//! `Allow` values for OPTIONS, generated from `src/routes.ts` (registration order).
pub const ROUTES: &[(&str, &str)] = &[
    ("/", "GET,HEAD"),
    ("/v1/entries/{entryId}", "GET,HEAD,POST,DELETE"),
    ("/private/entries/{entryId}", "GET,HEAD,POST,DELETE"),
    ("/v1/entries/{entryId}/meta", "GET,HEAD"),
    ("/private/entries/{entryId}/meta", "GET,HEAD"),
    ("/v1/entries/{entryId}/access-description", "GET,HEAD"),
    ("/v1/entries", "POST,GET,HEAD"),
    ("/private/entries", "POST,GET,HEAD"),
    ("/v1/check-entries-existence", "POST"),
    ("/private/entries/{entryId}/unversioned-data", "POST"),
    ("/private/entries/{entryId}/switch-revision", "POST"),
    ("/v1/entries/{entryId}/rename", "POST"),
    ("/private/entries/{entryId}/rename", "POST"),
    ("/v1/get-entries", "POST"),
    ("/private/v1/get-entries", "POST"),
    ("/v1/entries/{entryId}/revisions", "GET,HEAD"),
    ("/private/entries/{entryId}/revisions", "GET,HEAD"),
    ("/v1/entries/{entryId}/relations", "GET,HEAD"),
    ("/private/entries/{entryId}/relations", "GET,HEAD"),
    ("/v1/get-entries-relations", "POST"),
    ("/private/v1/get-entries-relations", "POST"),
    ("/v1/get-entries-data", "POST"),
    ("/v1/get-entries-meta", "POST"),
    ("/v1/get-entries-annotation", "POST"),
    ("/v1/locks/{entryId}", "GET,HEAD,POST,DELETE"),
    ("/private/locks/{entryId}", "GET,HEAD,POST,DELETE"),
    ("/v1/locks/{entryId}/extend", "POST"),
    ("/private/locks/{entryId}/extend", "POST"),
    ("/v1/states/{entryId}/{hash}", "GET,HEAD"),
    ("/v1/states/{entryId}", "POST"),
    ("/v1/favorites", "GET,HEAD"),
    ("/v1/favorites/{entryId}", "POST,DELETE"),
    ("/v1/favorites/{entryId}/rename", "POST"),
    ("/v2/workbooks/{workbookId}/entries", "GET,HEAD"),
    ("/private/v2/workbooks/{workbookId}/entries", "GET,HEAD"),
    ("/v2/workbooks/{workbookId}", "GET,HEAD,DELETE"),
    ("/private/v2/workbooks/{workbookId}", "GET,HEAD,DELETE"),
    ("/v2/workbooks", "GET,HEAD,POST"),
    ("/v2/workbooks-get-list-by-ids", "POST"),
    ("/v2/workbooks/{workbookId}/update", "POST"),
    ("/private/v2/workbooks/{workbookId}/update", "POST"),
    ("/private/v2/workbooks", "POST"),
    ("/v2/delete-workbooks", "DELETE"),
    ("/v2/workbooks/{workbookId}/move", "POST"),
    ("/v2/move-workbooks", "POST"),
    ("/v2/workbooks/{workbookId}/copy", "POST"),
    ("/private/v2/workbooks/{workbookId}/restore", "POST"),
    ("/private/all-workbooks", "GET,HEAD"),
    ("/v1/collections", "POST"),
    ("/private/v1/collections", "POST"),
    ("/v1/collections/{collectionId}", "GET,HEAD,DELETE"),
    ("/private/v1/collections/{collectionId}", "GET,HEAD"),
    ("/v1/collections-get-list-by-ids", "POST"),
    ("/v1/collection-content", "GET,HEAD"),
    ("/v1/structure-items", "GET,HEAD"),
    ("/v1/root-collection-permissions", "GET,HEAD"),
    ("/v1/collections/{collectionId}/breadcrumbs", "GET,HEAD"),
    ("/v1/delete-collections", "DELETE"),
    ("/v1/collections/{collectionId}/move", "POST"),
    ("/v1/move-collections", "POST"),
    ("/v1/collections/{collectionId}/update", "POST"),
    ("/v2/entries/{entryId}/copy", "POST"),
    ("/v2/copy-entries", "POST"),
    ("/v1/color-palettes", "GET,HEAD,POST"),
    ("/v1/color-palettes/{colorPaletteId}", "GET,HEAD,DELETE"),
    ("/v1/color-palettes/{colorPaletteId}/update", "POST"),
    ("/v1/tenants/set-default-color-palette", "POST"),
    ("/v1/tenants/settings", "POST"),
    ("/v1/tenants/details", "GET,HEAD"),
    ("/private/tenants/{tenantId}/details", "GET,HEAD"),
    ("/private/resolveTenantByEntryId", "GET,HEAD"),
    ("/private/resolve-tenant", "GET,HEAD"),
];

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// express answers `OPTIONS` automatically: 200, `Allow: <methods>`, body is the
/// same string, `Content-Type: text/html; charset=utf-8`. Unknown paths 404.
fn matches(pattern: &str, path: &str) -> bool {
    if pattern == "/" {
        return path == "/";
    }

    let pattern_parts: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let path_parts: Vec<&str> = path.trim_matches('/').split('/').collect();

    pattern_parts.len() == path_parts.len()
        && pattern_parts
            .iter()
            .zip(path_parts.iter())
            .all(|(pattern_part, path_part)| {
                (pattern_part.starts_with('{') && !path_part.is_empty())
                    || pattern_part == path_part
            })
}

pub fn allow_for(path: &str) -> Option<&'static str> {
    let path = path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };

    // Health endpoints are registered directly on the express app in
    // `src/index.ts`, outside the generated route table.
    if matches!(
        path,
        "/" | "/ping" | "/ping-db" | "/ping-db-primary" | "/pool"
    ) {
        return Some("GET,HEAD");
    }

    ROUTES
        .iter()
        .find(|(pattern, _)| matches(pattern, path))
        .map(|(_, allow)| *allow)
}

pub async fn handle_options(request: Request, next: Next) -> Response {
    if request.method() != Method::OPTIONS {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();

    match allow_for(&path) {
        Some(allow) => {
            let mut response = (StatusCode::OK, allow.to_string()).into_response();
            if let Ok(value) = HeaderValue::from_str(allow) {
                response.headers_mut().insert(header::ALLOW, value);
            }
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response
        }
        None => next.run(request).await,
    }
}
