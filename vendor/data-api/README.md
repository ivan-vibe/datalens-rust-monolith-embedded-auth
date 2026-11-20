# DataLens data-api — Rust (v4)

> **v4 publication bundle.** Build/test commands and the complete environment matrix are in
> [`../CONFIGURATION.md`](../CONFIGURATION.md) and [`../CACHING.md`](../CACHING.md).


> **v4:** internal MessagePack, metadata cache and chunked `/result-stream`; see [V4.md](V4.md).


> **v3 MessagePack variant:** image `datalens-data-api-rust-v3:0.2457.0`.
> See [V3.md](V3.md).


> **v2 performance variant:** image `datalens-data-api-rust-v2:0.2457.0`.
> The audit, measurements and the checklist verdict are in [V2.md](V2.md).


**Version 0.2457.0** · Rust · Apache-2.0

A Rust implementation of the DataLens **data-api** service. It targets the Python
`data-api` shipped with DataLens **0.2457.0** (DataLens release 2.9.0) and keeps the same
routes, ports, request/response bodies, error envelope, and `us` / PostgreSQL / ClickHouse
neighbours for the endpoints it implements.

> **This is Rust only.** One binary serves the API; formulas are compiled by the
> `dfl_native` crate in this repository and linked as a normal Rust library. There is no
> Python runtime, no `ctypes` bridge and no `dl_formula` in the request path.
>
> This is an independent implementation, not an official DataLens component. It covers a
> working vertical slice of the data path — see [Status](#status) before deploying it.

## Why

The Python service spends most of its time in request handling, formula translation,
type marshalling and object churn. The Rust port keeps the wire contract compatible
while removing the interpreter from the hot path: a demo-dashboard request that took
~131 ms at p50 in Python answers in ~8 ms here, and a release binary runs without a Python
image. `docs/PERFORMANCE.md` has the measurements.

## Architecture

```
HTTP (axum)
   │
   ├─ dl_api           request/response schemas, updates, legend, totals, preflight, errors
   ├─ dl_query         dataset + request -> SELECT (WHERE/GROUP BY/HAVING/ORDER BY/LIMIT)
   ├─ dl_core          dataset model, field types, validation, component errors
   ├─ dl_us            `us` client: GET /private/entries/{id} -> dataset, TTL cache
   ├─ dfl_native       formula compiler (Rust library, no FFI)
   ├─ dl_postgres      executor: tokio-postgres, rows as text
   └─ dl_clickhouse    executor: ClickHouse HTTP interface + JSONCompact, rows as text
```

One process serves datasets from different databases. `dl_us` resolves a dataset's
`connection` entry into a DSN whose scheme picks both the executor (`postgres://`,
`clickhouse://`, `clickhouses://`) and the SQL dialect; `DL_DIALECT` is only the fallback
for `DL_DATABASE_URL` and the local store.

## Status

This is a vertical slice, not a complete replacement. The v2 data path (datasets, legend,
SQL compilation, execution, pivot, preflight), NATIVE JWT authentication and PostgreSQL TLS
are implemented. Still missing: tenant scoping and per-object ACLs, caching, metrics, the
v1/v1.5 result bodies, `dashsql` and the extra connectors. `docs/PORTING.md` has the full inventory.

| Area | State |
|---|---|
| v2 `fields` / `preview` / `result` / `pivot` / `result-preflight` / `values/*` | implemented |
| dataset loading from `us` (entries, connections, fernet secrets, TTL cache) | implemented |
| SQL generation + formulas (PostgreSQL, ClickHouse, MySQL dialect) | implemented |
| PostgreSQL and ClickHouse execution | implemented |
| legend, roles, filters, totals, pivot incl. row/column totals | implemented |
| MySQL executor | missing (dialect only) |
| other connectors (MSSQL, Oracle, BigQuery, ...) | missing |
| v1 / v1.5 `result` bodies, `dashsql`, `typed_query` | missing |
| authentication | `NONE` and `NATIVE` (JWT PS256/RS256); `ZITADEL` fail closed |
| RLS (`user`/`all`/`userid` rules) | implemented; group rules need a `groups` JWT claim |
| PostgreSQL TLS (`ssl_enable`) | implemented: `sslmode=require/prefer`, rustls, private CA |
| query cache | request-scoped memo only; no cross-request cache |
| metrics, tracing export, Sentry | missing |
| CSV / XLSX export, streaming | missing |

`AGO` / `AT_DATE` are unsupported on purpose: in `dl_formula` they only resolve in the
`mutation/` stage, which is not ported.

## Build and test

Requires a stable Rust toolchain (edition 2021) and a C compiler for the bundled `zstd`
and `ring` sources. TLS is `rustls`; PostgreSQL is reached through `tokio-postgres`.

```bash
cargo test --workspace --locked      # unit + HTTP contract tests, no database required
cargo build --release --locked       # target/release/datalens-server
cargo fmt --all --check
```

The test suite covers the HTTP contract end to end with an in-process axum router: it
starts from a request body, compiles SQL, and asserts the response envelope, so it needs
no PostgreSQL or ClickHouse instance.

## Run

The binary is configured through environment variables and listens on `DL_PORT`
(default `8000`).

```bash
# Local store only: the built-in `demo` dataset, SQL compiled, no rows without a database.
DL_PORT=8099 ./target/release/datalens-server

# PostgreSQL fallback for datasets that do not resolve their own connection.
DL_DATABASE_URL=postgres://user:pass@localhost/db DL_DIALECT=postgres \
  DL_PORT=8099 ./target/release/datalens-server

# Datasets and connections from `us` (the container's mode).
US_HOST=http://us:8080 US_MASTER_TOKEN=us-master-token \
  DL_CRY_KEY_VAL_ID_KEY=... DL_PORT=8099 ./target/release/datalens-server

# NATIVE auth: every data route needs `Authorization: Bearer <jwt>`.
AUTH__TYPE=NATIVE AUTH__JWT_ALGORITHM=PS256 AUTH__JWT_KEY="$(cat public.pem)" \
  US_HOST=http://us:8080 DL_PORT=8099 ./target/release/datalens-server
```

With no database configured, requests still compile SQL and answer `200` with empty
`result_data` — useful for smoke tests and for inspecting `blocks[0].query`.

`datalens-server --healthcheck /ping_ready` probes a local instance over TCP and exits `0`
on `200`; it is the container healthcheck and needs neither `curl` nor a shell.

`AUTH__TYPE=NONE` is the default; `NATIVE` requires a valid JWT on every data route.
Unset logs a warning; `ZITADEL` makes the process exit rather than serve every caller as the
`us` master user.

## HTTP API

| Method | Path |
|---|---|
| `GET` | `/ping`, `/ping_ready` |
| `GET` | `/api/data/v2/datasets/{ds_id}/fields` |
| `GET` | `/api/data/v1/datasets/{ds_id}/fields` (legacy alias used by the charts engine) |
| `POST` | `/api/data/v2/datasets/data/preview` (inline dataset) |
| `POST` | `/api/data/v2/datasets/{ds_id}/preview` |
| `POST` | `/api/data/v2/datasets/{ds_id}/result` |
| `POST` | `/api/data/v2/datasets/{ds_id}/pivot` |
| `POST` | `/api/data/v2/datasets/{ds_id}/result-preflight` |
| `POST` | `/api/data/v2/datasets/{ds_id}/values/distinct` |
| `POST` | `/api/data/v2/datasets/{ds_id}/values/range` |
| `POST` | `/api/data/v1/datasets/{ds_id}/versions/draft/preview` (and `/api/v1/...`) |
| `POST` | `/api/data/v1.5/datasets/{ds_id}/preview` |

The dataset id is resolved through `us` when `US_HOST` is set, otherwise from the local
store: `DL_DATASETS` (a JSON map `{"ds_id": <dataset>}`) plus the built-in `demo`.
`us` answers are cached for `DL_US_CACHE_TTL_SECS`.

```bash
curl -s localhost:8099/api/data/v2/datasets/demo/result \
  -H 'content-type: application/json' -d '{
    "fields": [
      {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
      {"ref": {"type": "id", "id": "f_sales"},  "role_spec": {"role": "measure"}}
    ],
    "filters": [{"ref": {"type": "id", "id": "f_region"}, "operation": "contains", "values": ["Eur"]}],
    "autofill_legend": true
  }'
```

The response is `DataApiV2ResponseSchema`:

```json
{
  "fields": [{"legend_item_id": 0, "id": "f_region", "title": "Region",
              "data_type": "string", "field_type": "DIMENSION", "item_type": "field"}],
  "result_data": [{"rows": [{"data": ["Europe", 42], "legend": [0, 1]}]}],
  "blocks": [{"block_id": 0, "query": "SELECT t0.\"Region\" AS \"f_region\", ..."}],
  "notifications": [],
  "data_export": {"background": {"allowed": false, "reason": ["PROHIBITED_EXPORT_TENANT"]},
                  "basic": {"allowed": true, "reason": null}},
  "data_export_forbidden": false
}
```

The demo tenant denies background export; a tenant that allows it reports both entries as
allowed. `rows` is empty when no database is configured, but `blocks[0].query` still carries
the compiled SQL.

Errors are `{"code": ..., "message": ...}` with the matching status, e.g.
`ERR.DS_API.REQUEST.UNKNOWN_FIELD` (400) or `ERR.DS_API.DB.DATASET_NOT_FOUND` (404).

Answers are compressed (`zstd`, `br`, `gzip`) when the client sends `Accept-Encoding`,
and can be returned as `application/msgpack` when the client asks for it with `Accept`.

### What the data path does

* **Datasets** — the `us` JSON (`result_schema`, `source_collections`, `source_avatars`,
  parameters, `cast`/`data_type`, `DIMENSION`/`MEASURE`), inline datasets in a request
  body, and the local store.
* **Request updates** — `add_field`, `update_field`, `delete_field`; anything else is
  `ERR.DS_API.ACTION_NOT_ALLOWED`. `update_field` is a partial patch; `new_id` renames a
  field; a missing `guid` becomes a readable id.
* **Legend** — roles (`row`, `measure`, `info`, `template`, `parameter`, `total`, ...),
  id assignment, explicit `legend_item_id` reservation, `autofill_legend`, duplicates
  rejected with `ERR.DS_API.LEGEND.NON_UNIQUE_IDS`.
* **Queries** — dimensions, measures (`sum/avg/min/max/count/countunique`), formulas
  through `dfl_native`, `WHERE` (all `WhereClauseOperation`s except `len*`), `GROUP BY`,
  `HAVING` for aggregated filters, `ORDER BY`, `LIMIT`/`OFFSET`, per-dialect quoting and
  literals, row-count hard limits, `disable_group_by`.
* **Totals** — `{"result": {"with_totals": true}}` builds and runs a second query; the
  totals row is kept off the v2 wire exactly like the Python service.
* **Pivot** — `Measure Names` / `Dimension Names`, both axes, pagination, multi-block
  totals (row totals, column totals, intersections).
* **Execution** — pooled `tokio-postgres` (`simple_query`, rows as text, typed decoding
  into JSON numbers/booleans), ClickHouse HTTP + `JSONCompact`, per-dataset DSNs resolved
  from `us` connections.
* **Authentication and RLS** — `AUTH__TYPE=NATIVE` validates the JWT (`userId`, `exp`,
  `roles`); the compiler turns the dataset's `user`/`all`/`userid` rules into anonymous `IN`
  filters. `us` is still read with the service master token, and per-object ACLs stay with
  UI/control-api.
* **Transport** — PostgreSQL TLS through rustls (`sslmode=require/prefer`, private CA via
  `DL_PG_CA_FILE`); `us` and ClickHouse HTTPS through the same root set (`DL_CA_FILE`).
* **Post-processing** — array literals, Python-compatible float spelling, data export
  flags.

## Configuration

| Variable | Meaning |
|---|---|
| `DL_PORT` / `PORT` | listen port, default `8000` |
| `AUTH__TYPE` / `AUTH_TYPE` | `NONE` (default) or `NATIVE` (JWT); `ZITADEL` makes the process exit |
| `AUTH__JWT_ALGORITHM` | `PS256` (default), `PS384`, `PS512`, `RS256`, `RS384`, `RS512` |
| `AUTH__JWT_KEY` | PEM public key for `NATIVE` (`\n` escapes accepted) |
| `DL_DATABASE_URL` / `DATABASE_URL` | fallback PostgreSQL DSN; **unset = each dataset's `connection` decides the database** |
| `DL_DIALECT` | `postgres` (default), `mysql`, `clickhouse`; a resolved `us` DSN's scheme wins |
| `DL_DATASETS` | path to a JSON map `{"ds_id": <dataset>}`; `demo` is always built in |
| `US_HOST` | `us` base URL (e.g. `http://us:8080`); **unset = local store only** |
| `US_MASTER_TOKEN` | `X-US-Master-Token` for `us` (the container's mode) |
| `DL_US_BRANCH` | `published` (default, what the UI reads) or `saved` |
| `DL_US_CACHE_TTL_SECS` | dataset cache TTL, default `30` |
| `DL_CONNECTION_CACHE_TTL_SECS` | connection (DSN) cache TTL, default `300` |
| `DL_DB_POOL_SIZE` | PostgreSQL connections per database, default `8` |
| `DL_PG_JIT` | `auto` (default), `off`, `on` — PostgreSQL JIT cost thresholds |
| `DL_CRY_KEY_VAL_ID_KEY` / `CONTROL_API_CRYPTO_KEY` | fernet key of the `us` secrets (connection passwords) |
| `DL_CRY_ACTUAL_KEY_ID` | key id the secrets name |
| `DL_DATA_EXPORT_BACKGROUND_ALLOWED` | `1` allows background export for the demo tenant |
| `DL_CA_FILE` | PEM bundle added to the HTTP clients' roots (fallbacks: `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`) |
| `DL_PG_CA_FILE` | PEM bundle for PostgreSQL TLS; falls back to `DL_CA_FILE` |
| `RUST_LOG` | `tracing` filter, e.g. `info`, `datalens_server=debug` |

## Security

* **Authentication is `NONE` or `NATIVE`.** `NATIVE` validates the JWT (`userId`, `exp`,
  `roles`) with the `AUTH__JWT_KEY` public key and gates every data route. `ZITADEL` is
  rejected.
* **RLS is applied by the query compiler.** `user`, `all` and `userid` rules become
  anonymous `IN` filters in every block (result, totals, pivot, preview, values). Group
  rules match only when the JWT carries a `groups` claim; the open-source auth service does
  not issue one. `us` is still read with the service master token, and per-object ACLs are
  left to the UI/control-api; do not treat data-api as the object-permission boundary.
* **PostgreSQL TLS is supported.** `dl_postgres` uses rustls for `sslmode=require`/`prefer`;
  set `DL_PG_CA_FILE` (or `DL_CA_FILE`) for a private CA. `sslmode=disable` remains plain
  TCP only when the deployment asks for it.
* **Private CA for HTTP clients** works through `DL_CA_FILE`; `us` and ClickHouse HTTPS
  trust it.
* **The production image has no shell**, so incident response is logs and metrics only. The
  `Dockerfile.debug` image exists for tests and operations.

`SECURITY.md` has the full statement.

## Docker

```bash
docker build -t datalens-data-api-rust-v2:0.2457.0 .
docker run --rm -p 8080:8080 -e AUTH__TYPE=NONE -e RUST_LOG=info \
  datalens-data-api-rust-v2:0.2457.0

# Debug/operations image: shell, curl, psql, dig, openssl (not for production).
docker build -f Dockerfile.debug -t datalens-data-api-rust-v2:0.2457.0-debug .
```

The runtime image is `scratch` with one statically linked musl binary: no shell, no system
libc, no package manager and no CA bundle. It is **14.9 MB on disk / ~4.2 MB compressed** (the
binary is 10.5 MB); the earlier `debian:bookworm-slim` image was 178 MB. TLS uses the webpki
roots compiled into the binary (`reqwest`'s `rustls-tls`), so public CAs work without files.
A private CA works too: mount the bundle and point the HTTP clients at it — no rebuild:

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/certs/ca.crt:/certs/ca.crt:ro" \
  -e AUTH__TYPE=NONE -e DL_CA_FILE=/certs/ca.crt \
  datalens-data-api-rust-v2:0.2457.0
```

`DL_CA_FILE` is read by the `us` and ClickHouse clients; `SSL_CERT_FILE` and
`REQUESTS_CA_BUNDLE` are fallbacks. PostgreSQL TLS is outside this path (see
[Security](#security)).

| image | base | shell / tools | CA | size |
|---|---|---|---:|---:|
| `datalens-data-api-rust-v2:0.2457.0` | `scratch` | none | webpki + `DL_CA_FILE` | 14.9 MB |
| `datalens-data-api-rust-v2:0.2457.0-debug` | Alpine | bash, curl, psql, dig, openssl | `ca-certificates` + `DL_CA_FILE` | 68.0 MB |

The default image is the deployment image. The debug image is for `docker exec` and
network/DB diagnostics; it is intentionally not minimal.

The healthcheck is built in — `datalens-server --healthcheck /ping_ready` — so the image
needs no `curl`. BuildKit sets `TARGETARCH`; without it the Dockerfile falls back to
`uname -m`. Both `--build-arg TARGETARCH=arm64|amd64` and `--build-arg CARGO_BUILD_JOBS=N`
can override the defaults. The image exposes port `8080` and answers `GET /ping_ready`, so
it can replace the `data-api` service in a DataLens Compose stack without changing
`BI_DATA_ENDPOINT`.

To run it in a real stack, use the upstream DataLens Compose file with the overlay in
[`deploy/docker-compose.rust.yaml`](deploy/docker-compose.rust.yaml); it replaces only the
`data-api` service and sets the no-authentication defaults of its neighbours:

```bash
git clone https://github.com/datalens-tech/datalens.git repo
AUTH_ENABLED=false US_MASTER_TOKEN=us-master-token \
  docker compose --project-directory repo \
    -f repo/docker-compose.yaml -f deploy/docker-compose.rust.yaml up -d
```

[`deploy/README.md`](deploy/README.md) covers the private-CA mount and the environment.

## Repository layout

```
crates/
  datalens-server/   axum routes, config, executors, connection resolution
  dl_api/            request/response schemas, updates, legend, pivot, preflight
  dl_query/          SQL compilation, fork/window handling, literals
  dl_core/           dataset model, field types, validation
  dl_us/             `us` client, entries, connections, fernet, caches
  dl_postgres/       PostgreSQL executor
  dl_clickhouse/     ClickHouse executor
dfl_native/          formula compiler + `dfl-cli` debugging binary
Dockerfile           static musl binary on `scratch` (14.9 MB image)
Dockerfile.debug     same binary on Alpine with shell/curl/psql for operations
deploy/              Compose overlay and deployment notes
docs/                porting inventory and performance notes
SECURITY.md          auth, RLS and transport-security statement
```

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). DataLens and `dl_formula` are
DataLens project components, also licensed under Apache-2.0.
