//! Minimal Swagger-compatible `/api-docs` (v2).
//!
//! The Node service mounts swagger-ui with a generated OpenAPI document. We do
//! not bundle the ~2 MB of UI assets; instead `/api-docs/` is a small
//! self-contained viewer and `/api-docs/swagger.json` is a generated OpenAPI
//! 3.0 document with every route (methods/paths from `api::options::ROUTES`).

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::api::options::ROUTES;

/// `GET /api-docs` → 301 `/api-docs/` (express' static middleware redirect).
pub async fn redirect() -> Response {
    let mut response = StatusCode::MOVED_PERMANENTLY.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/api-docs/"));
    response
}

pub async fn index() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>DataLens US API</title>
<style>body{font:14px/1.4 -apple-system,Segoe UI,Roboto,sans-serif;margin:2rem;max-width:1100px}
code{background:#f4f4f4;padding:.1rem .3rem;border-radius:3px}td,th{border-bottom:1px solid #eee;padding:.35rem .6rem;text-align:left}</style>
</head><body>
<h1>DataLens United Storage API</h1>
<p>OpenAPI document: <a href="/api-docs/swagger.json">/api-docs/swagger.json</a></p>
<table id="routes"><thead><tr><th>Method</th><th>Path</th></tr></thead><tbody></tbody></table>
<script>
fetch('/api-docs/swagger.json').then(r=>r.json()).then(spec=>{
  const body=document.querySelector('#routes tbody');
  const rows=[];
  for (const [path, ops] of Object.entries(spec.paths||{}))
    for (const method of Object.keys(ops)) rows.push([method.toUpperCase(), path]);
  rows.sort((a,b)=>a[1].localeCompare(b[1])||a[0].localeCompare(b[0]));
  for (const [m,p] of rows){ const tr=document.createElement('tr');
    tr.innerHTML='<td><code>'+m+'</code></td><td><code>'+p+'</code></td>'; body.appendChild(tr); }
});
</script></body></html>"#,
    )
}

/// OpenAPI 3.0 skeleton generated from the route table.
pub async fn spec() -> Json<Value> {
    let mut paths: Map<String, Value> = Map::new();

    for (pattern, allow) in ROUTES {
        let path = pattern.replace('{', "{").replace('}', "}");
        let mut operations = Map::new();

        for method in allow.split(',') {
            if method == "HEAD" || method.is_empty() {
                continue;
            }
            operations.insert(
                method.to_ascii_lowercase(),
                json!({
                    "operationId": format!("{}_{}", method.to_ascii_lowercase(), sanitize(pattern)),
                    "summary": format!("{method} {pattern}"),
                    "tags": [tag_for(pattern)],
                    "parameters": path_parameters(pattern),
                    "responses": {
                        "200": {"description": "OK"},
                        "400": {"description": "VALIDATION_ERROR"},
                        "404": {"description": "NOT_EXIST_ENTRY / NOT_FOUND"}
                    }
                }),
            );
        }

        paths.insert(path, Value::Object(operations));
    }

    Json(json!({
        "openapi": "3.0.3",
        "info": {
            "title": "DataLens United Storage",
            "version": crate::metrics::VERSION,
            "description": "Rust port of datalens-us (drop-in compatible HTTP API)."
        },
        "servers": [{"url": "/"}],
        "paths": paths
    }))
}

fn sanitize(pattern: &str) -> String {
    pattern
        .trim_matches('/')
        .replace(['/', '{', '}'], "_")
        .trim_matches('_')
        .to_string()
}

fn tag_for(pattern: &str) -> &'static str {
    if pattern.contains("/entries") {
        "entries"
    } else if pattern.contains("/workbooks") {
        "workbooks"
    } else if pattern.contains("/collections") {
        "collections"
    } else if pattern.contains("/locks") {
        "locks"
    } else if pattern.contains("/favorites") {
        "favorites"
    } else {
        "other"
    }
}

fn path_parameters(pattern: &str) -> Value {
    let mut parameters = Vec::new();
    for segment in pattern.split('/') {
        if let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            parameters.push(json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": {"type": "string"}
            }));
        }
    }
    Value::Array(parameters)
}
