# Changelog

## v3 schema metadata cache

* Field types/data types are cached by an exact schema fingerprint; an edit changes the key,
  so no stale schema is served. Bounded map, no TTL.

## v3 MessagePack internal transport

* charts-engine -> data-api and data-api -> us-rust negotiate `application/msgpack`; JSON
  remains the browser/control-api contract.
* Internal hops use `Accept-Encoding: identity`; zstd stays on the final JSON answer only.
* us-rust converts successful JSON responses to MessagePack for clients that ask for it.
* PostgreSQL remains on the text protocol; binary `postgres-types` is not part of v3.

## v2 performance pass

* Empty `updates` borrow the dataset instead of deep-cloning it; actual edits still copy.
* `field_types` and `field_data_types` are computed in one resolver pass (used by result and pivot).
* RLS-free datasets no longer clone the `QuerySpec` before compiling.
* The PostgreSQL pool clears stale idle connections and retries once after a database restart.
* Request-scoped SQL memo: identical SQL inside one execution is run once; concurrent duplicates
  share a per-key async gate. It is dropped with the request, so no stale data is ever served
  across requests.
* Request tracing at target `dl_timing`: total/work/wait plus per-stage detail (`auth`, `dataset`,
  `dsn`, `prepare.*`, `db`, `decode`, `merge`, `pivot`, `response`); no global state.

## 0.2457.0 — first public snapshot

Rust implementation of the DataLens data-api HTTP contract, targeting the Python
`datalens-data-api:0.2457.0` (DataLens 2.9.0) release.

* v2 data path: `fields`, `preview`, `result`, `pivot`, `result-preflight`,
  `values/distinct`, `values/range`.
* Legacy v1/v1.5 preview routes (the dataset editor's `getPreview`).
* Datasets from `us` (entries, connections, fernet secrets, TTL caches) or a local store.
* SQL compilation for PostgreSQL, ClickHouse and MySQL; formulas compiled by `dfl_native`.
* PostgreSQL (`tokio-postgres`) and ClickHouse (HTTP + `JSONCompact`) executors.
* Legend, roles, filters, totals blocks, pivot with row/column totals.
* zstd/br/gzip compression and opt-in `application/msgpack`.
* Minimal runtime image: statically linked musl binary on `scratch` — 14.9 MB on disk /
  ~4.2 MB compressed, no shell or CA bundle, built-in `--healthcheck`.
* `Dockerfile.debug`: the same binary on Alpine with shell, `curl`, `psql`, `dig` and
  `openssl` for tests and operations.
* Private CA for the HTTP clients (`us`, ClickHouse) through `DL_CA_FILE` /
  `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE`; no rebuild needed.
* `AUTH__TYPE=NATIVE`: PS256/RS256 JWT validation with the `dl_auth_native` claims
  (`userId`, `exp`, `roles`); `NONE` stays the default and `ZITADEL` fails closed.
* Row-level security: `user`, `all` and `userid` rules become anonymous `IN` filters in
  result, totals, pivot, preview and values queries; group rules need a `groups` claim.
* PostgreSQL pool survives a database restart: a closed idle connection is dropped, all
  suspect idle connections are cleared, and the query is retried once on a fresh connection.
* PostgreSQL TLS through rustls (`sslmode=require/prefer`, `DL_PG_CA_FILE` / `DL_CA_FILE`);
  private CAs need no rebuild.
* `deploy/docker-compose.rust.yaml` and `SECURITY.md`: the supported environment, the
  no-auth Compose overlay, and the auth/RLS/transport-security statement.
