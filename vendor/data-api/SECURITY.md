# Security

This build is an independent Rust implementation of the DataLens data-api. It implements the
two auth modes the upstream Compose file uses (`NONE` and `NATIVE`) and PostgreSQL TLS, but it
is not a complete security-equivalent replacement for the Python service yet. The gaps below
are deliberate and visible; do not deploy it where they matter.

## Authentication and tenancy

* **`AUTH__TYPE=NONE`** is the default. No token is required; the `us` master token is used
  for every dataset and connection read. This is the upstream `AUTH_ENABLED=false` mode.
* **`AUTH__TYPE=NATIVE`** requires `Authorization: Bearer <jwt>` on every route except
  `/ping` and `/ping_ready`. The JWT is validated with `AUTH__JWT_KEY` (PEM public key) and
  `AUTH__JWT_ALGORITHM` (default `PS256`; `PS384`, `PS512`, `RS256`, `RS384`, `RS512` are
  accepted). Claims: `userId`, `exp`, `roles` — the same payload `dl_auth_native` reads.
* With `NATIVE`, the JWT gates every data route and its `userId` is used for RLS. `us` is
  still read with the service master token, and per-object ACLs are left to the
  UI/control-api.
* **`AUTH__TYPE=ZITADEL` is not implemented.** The process exits for it rather than serve
  requests unauthenticated.
* **RLS is applied for `user`, `all` and `userid` rules.** The compiler adds anonymous
  `IN` filters to result, totals, pivot, preview and values queries. Group rules match only
  when the JWT carries a `groups` claim (the open-source auth service does not issue one).
  There is no per-object ACL inside data-api; `us` stays the service-level authority.
* **No tenant headers are derived.** The service does not synthesize `x-dl-*` tenancy
  context; the upstream gateway and `us` remain the authority.

## Transport security

* **HTTP clients (`us`, ClickHouse)** verify certificates against the webpki roots compiled
  into the binary. A private CA is supported without a rebuild: mount the PEM and set
  `DL_CA_FILE` (or `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE`).
* **PostgreSQL TLS is supported.** A `us` connection whose `ssl_enable` is set gets
  `?sslmode=require` in its DSN; `DL_DATABASE_URL` may also carry `sslmode=require`
  (`verify-full` / `verify-ca` are normalised to `require` because rustls verifies the chain
  and hostname). The CA comes from `DL_PG_CA_FILE` (fallback `DL_CA_FILE`). `sslmode=disable`
  uses plain TCP only when explicitly configured.
* **ClickHouse HTTPS** (`clickhouses://`) is supported by the HTTP client and uses the same
  root set.
* The `scratch` image carries no CA bundle files; the `Dockerfile.debug` image does, for
  `curl`/`openssl` diagnostics. Both use `DL_CA_FILE` / `DL_PG_CA_FILE` for private
  certificates.

## Secrets

`US_MASTER_TOKEN`, `DL_CRY_KEY_VAL_ID_KEY` / `CONTROL_API_CRYPTO_KEY`, the fernet key id,
`AUTH__JWT_KEY` (public key only) and the database credentials arrive through the
environment, as in the upstream Compose file. Nothing is written to disk. The container runs
as UID/GID `65532` (`10001` in the debug image) and has no writable layer requirement.

## Image contents

The production image is `scratch` with a statically linked musl binary. There is no shell,
package manager, `curl` or `ca-certificates`; the healthcheck is the binary itself
(`--healthcheck /ping_ready`). The debug image adds those tools for operations only.

## Reporting

This is a port tracked against the upstream DataLens 0.2457.0 contract. Report a security
issue in the Rust code through the repository's private security advisory channel; report
an upstream issue to the DataLens project.
