# us-rust — US metadata service (v4)

Rust implementation of the DataLens US metadata service. DataLens target: `0.2457.0`; service
version `1.196`.

## Responsibilities

- entries: read/write/copy/lock, revisions, relations;
- datasets, workbooks, collections, tenants, palettes, favorites;
- connection metadata and permissions;
- JSON for the UI/control-api; optional MessagePack for internal callers.

The data-api in v4 requests **JSON** by default (`DL_US_MSGPACK=0` in data-api): US payloads are
small, and the MessagePack layer has to buffer JSON, parse it and re-encode it. The old
`Accept: application/msgpack` path remains available for compatibility.

## Build

```bash
docker build -t rust-v4-us:1.196 .
```

## Test

```bash
CARGO_TARGET_DIR=/tmp/v4-us-target cargo test
```

## Configuration

See [`../CONFIGURATION.md`](../CONFIGURATION.md) for all US environment variables:
`US_RS_WORKERS`, `US_RS_HTTP_LOG`, `US_RS_REQUEST_TIMEOUT_MS`, `US_RS_MIGRATE`,
`US_RS_SKIP_SCHEMA_CHECK`, `AUTH_ENABLED`, `AUTH_TOKEN_PUBLIC_KEY`, and the `DL_*_HEADER`
overrides.

## Freshness

US entries are `Cache-Control: no-store` and have no ETag. Do not cache mutable latest entries.
See [`../CACHING.md`](../CACHING.md).
