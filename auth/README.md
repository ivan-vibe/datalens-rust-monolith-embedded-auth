# datalens-auth-rs

Standalone Rust replacement for the DataLens auth container
(`ghcr.io/datalens-tech/datalens-auth:0.27.0`) — **no container required**. One binary, one
`pg-auth-db` schema, the same PS256 keys and HTTP protocol, so mono-ui, control-api,
meta-manager, US and the data-api keep working unchanged.

It covers the full `datalens-auth:0.27.0` public surface (login + users + user management),
not only the login subset. See the repository README for how it is deployed (a separate
process in the main repository; linked into the binary in the embedded-auth fork).

## What it implements

| Route | Behaviour |
|---|---|
| `POST /signin` | login/password → scrypt verify, session + refresh-token rows, PS256 JWT pair, `auth`/`auth_exp` cookies. Bad credentials/invalid body → `302 /signin-fail`; malformed JSON → `400 Invalid JSON supplied` |
| `POST /refresh` | refresh-token rotation in one transaction (delete old row, insert new, re-read roles, sign new pair, update session IP). Missing token → `401 AUTH.NEED_RESET` "No refreshToken"; invalid/stale/expired → "Can't refresh tokens" |
| `GET /logout` | deletes the session (cascade removes the refresh token) when the presented refresh token is the current one, clears the four cookies, `200 {"done":true}` |
| `GET /v1/users/me/profile` | validates the Bearer access token, returns the exact `{"profile":{...}}` shape used by mono-ui's `ui-auth` on every page load |
| `POST /signup` | full implementation (zod-equivalent validation, user + default role + session in one transaction); disabled by default with the reference **403** `{"message":"Signup is disabled","code":"AUTH.SIGNUP_DISABLED"}` |
| `GET /ping`, `GET /ping-db`, `GET /ping-db-primary`, `GET /signin-fail`, `GET /` | reference bodies; `GET /` is auth-protected like the Node route |
| `GET /v1/users/list` | page/pageSize/filterString/idpType/roles query semantics (case-insensitive `%`/`_`-escaped LIKE over login/email/first/last, role and IdP filters), `{nextPageToken?, users}` |
| `POST /v1/users/get-by-ids` | decoded-id array (1…1000, duplicates preserved, missing ids omitted), `{users}` |
| `POST /v1/users/me/profile`, `POST /v1/users/me/password` | self-service profile patch / password change (old password verified), `ManageLocalUsers` route check |
| `GET/POST /v1/management/users/:userId/profile` | profile read / patch; `Manage` permission, write route adds `ManageLocalUsers` |
| `POST /v1/management/users/:userId/password` | password replacement (no old password), `Manage` + `ManageLocalUsers` |
| `POST /v1/management/users/create`, `DELETE /v1/management/users/:userId` | user create (zod-equivalent validation, default/viewer or explicit roles) and delete, `Manage` + `ManageLocalUsers` |
| `POST /v1/management/users/roles/{add,update,remove}` | role upsert / rename (with `ROLE_NOT_EXISTS` / `NOT_CONSISTENT` errors) / delete |
| `POST|DELETE /private/v1/management/users/...` | master-token variants of create/delete/profile/password/roles; `x-dl-auth-master-token` must equal one of `MASTER_TOKEN`'s values; IdP-user guards are bypassed like the Node `isPrivateRoute` path |

Permission semantics match the Node middleware: `InstanceUse` = any known `datalens.*` role,
`Manage` = token role `datalens.admin` **and** the database still grants it
(`introspectUserPermission`); `ManageLocalUsers` is a separate 403 check; private routes only
need the master token. IdP-managed users (`auth_users.idp_slug IS NOT NULL`) cannot be deleted
or have profile/password changed over the public routes (`403 IDP_USER_CHANGE_NOT_ALLOWED`) but
are managed through `/private/*`.

Everything else answers Express-style **404** (including wrong methods, e.g. `GET /signin`),
not 405. The `/pool` introspection route is not implemented — nothing in this deployment calls
it (control-api/meta-manager/US/data-api validate JWTs locally with `AUTH_TOKEN_PUBLIC_KEY`).

Protocol details reproduced byte-for-byte where it matters:

- Permission checks run after Bearer auth and before controller validation (so a viewer gets
  `403 AUTH.ACCESS_DENIED` before a `400` body error), exactly like the Node middleware order.
- Path/body ids use the lenient `decodeId` semantics of the API (no length/rotation check, so
  `GET /v1/management/users/123/profile` is `404`, not `400`); JWT claims keep the strict one.
- Cookie `auth` = URL-encoded JSON `{"accessToken","refreshToken"}`, `HttpOnly`;
  `auth_exp` = access-token `exp`. Both: `Max-Age=867600` (refresh TTL + 1 h), `Path=/`,
  `Expires=…`, `SameSite=Strict`, no `Domain` (`AUTH_COOKIE_*` override these).
- Logout clears four cookies exactly as `res.clearCookie` does (two with attributes, two bare).
- Access token claims: `userId`, `sessionId`, `roles`, `iat`, `exp` (`exp = iat + 900`);
  refresh token claims: `refreshTokenId`, `userId`, `sessionId`, `iat`, **no `exp`**.
  Ids are the 13-char encoded form (`encodeId`) shared with US/control-api.
- Passwords: Node-compatible scrypt (`N=16384, r=8, p=1, keylen=64`, `base64url(salt):base64url(key)`,
  constant-time compare).
- `Set-Cookie` attribute order matches the `cookie` npm package, so diffs against the Node
  service are empty apart from token values and timestamps.
- expresskit response headers: `Cache-Control: no-store, max-age=0, must-revalidate,
  proxy-revalidate`, `Surrogate-Control: no-store`, `x-request-id` (incoming value preserved).
- `x-forwarded-for`/`x-client-ip`/`x-real-ip`… IP extraction and `user-agent` (default
  `Unknown`) match the Node service; both are stored on `auth_sessions`.

## PostgreSQL pool resilience

`pg-auth-db` is shared with the Python cluster and can be restarted independently. The
`Db` pool discards idle clients whose driver observed the socket close (`Client::is_closed`)
and reconnects on the next request, so signin/profile/refresh and workbook import keep
working after a shared-Postgres restart. This was verified live: restart
`datalens-python-postgres-1`, then sign in through mono-ui again.

## Configuration

| Env | Default | Notes |
|---|---|---|
| `AUTH_PORT` / `PORT` | `8097` | listen address is `0.0.0.0` |
| `TOKEN_PRIVATE_KEY` | — **required** | PEM, PS256 signing key (same pair as the Node auth) |
| `TOKEN_PUBLIC_KEY` | — **required** | PEM public key (the one every other service validates) |
| `AUTH_POSTGRES_DSN` | `postgres://pg-user:postgres@127.0.0.1:28185/pg-auth-db` | shared `pg-auth-db` |
| `AUTH_ACCESS_TOKEN_TTL` | `900` | seconds |
| `AUTH_REFRESH_TOKEN_TTL` | `864000` | 10 days |
| `AUTH_SESSION_TTL` | `2592000` | 30 days |
| `AUTH_COOKIE_NAME` | `auth` | |
| `AUTH_COOKIE_MAX_AGE` | refresh TTL + 3600 | seconds |
| `AUTH_COOKIE_SAME_SITE` | `strict` | `strict` / `lax` / `none` |
| `AUTH_COOKIE_SECURE` | `false` | set `true` behind HTTPS |
| `AUTH_COOKIE_DOMAIN` | unset | no `Domain` attribute (matches `DISABLE_WILDCARD_COOKIE=true`) |
| `AUTH_SIGNUP_DISABLED` | `true` | production-safe default, same as native compose |
| `AUTH_MANAGE_LOCAL_USERS_DISABLED` | `false` | signup route check parity |
| `AUTH_DEFAULT_ROLE` | `datalens.viewer` | assigned on signup / create without roles |
| `AUTH_MASTER_TOKEN` / `MASTER_TOKEN` | unset | comma-separated list for `/private/v1/*`; unset disables them with the reference `403 {"error":"No master token in config"}` |
| `AUTH_DB_POOL_SIZE` | `4` | idle connections kept by the process |
| `RUST_LOG` | `info` | |

PEM values may be single-line with literal `\n` escapes (as Docker Compose passes them); both
forms are accepted.

## Build & run

```bash
cd new-test-monilte-v9/auth-rs
cargo build --release
./run.sh                     # dev: falls back to the reference container's PS256 keys
```

In this sandbox the crates were fetched into a workspace-local cargo home, so the build
command here was:

```bash
cargo build --release
```

Production run (explicit env, no Docker dependency):

```bash
export TOKEN_PRIVATE_KEY="$(cat /etc/datalens/auth-private.pem)"
export TOKEN_PUBLIC_KEY="$(cat /etc/datalens/auth-public.pem)"
export AUTH_POSTGRES_DSN='postgres://pg-user:postgres@127.0.0.1:28185/pg-auth-db'
export AUTH_PORT=8097
./target/release/datalens-auth-rs
```

systemd example (host deployment next to the new-test monolith):

```ini
[Unit]
Description=DataLens auth (Rust)
After=network-online.target

[Service]
ExecStart=/opt/datalens/auth-rs/datalens-auth-rs
EnvironmentFile=/etc/datalens/auth-rs.env
Restart=on-failure
RestartSec=2
# scrypt uses ~16 MiB per hash; no special limits needed beyond the default.
LimitNOFILE=4096

[Install]
WantedBy=multi-user.target
```

`/etc/datalens/auth-rs.env` holds the variables from the table above (`KEY=VALUE`, PEMs can be
single-line with `\n`). If the service restarts, existing sessions survive — they live in
`pg-auth-db`, shared with the Node service during cutover.

## Tests

Unit tests (claims/cookies/id codec/Node scrypt vector/zod-equivalent validation/issue
ordering/IP extraction), no services needed:

```bash
cargo test          # 22 tests
```

Live differential suite against the reference service and the shared database:

```bash
python3 tests/parity_live.py
```

The script builds `target/release/datalens-auth-rs` if needed, starts it on `:8097` (signup
enabled) and a second instance on `:8098` (signup disabled) with the reference container's
keys, then compares against `datalens-python-auth-1` on `:28187`. If either port is already
taken (for example by a manually started instance), a free ephemeral port is used instead:

- status/body/headers/`Set-Cookie` structure for every implemented route (incl. 404 + 405-vs-404);
- JWT header/claims/`exp-iat`, DB session and refresh-token rows (UA, IP, 30d/10d expiries,
  `auth_get_id()` id ↔ claim encoding);
- **cross-acceptance**: Node-issued access tokens and refresh cookies work on the Rust service
  and vice versa (profile, refresh rotation in both directions, stale-token rejection);
- logout DB cascade and stale-cookie rejection;
- signup validation-error parity, scratch-user signup parity + duplicate 409, then cleanup;
- user management: list paging/filters/LIKE-escaping and validation issues, get-by-ids
  (duplicates/order/missing ids), me profile/password, management profile/password/create/
  delete, roles add/update/remove and their error envelopes, permission failures
  (viewer/role-less), IdP-user guards (public 403 vs private success), private master-token
  variants (missing/wrong/substring tokens, Bearer-only, master-only), wrong-method 404s.

Latest run: **276/276 checks passed — no differences against the reference service**.
Scratch users/sessions are deleted in `finally`; `AUTH_RS_KEEP_DATA=1` skips cleanup.
Env overrides: `AUTH_RS_REF`, `AUTH_RS_TEST_PORT`, `AUTH_RS_DISABLED_PORT`,
`AUTH_RS_PG_CONTAINER` (default `datalens-python-postgres-1`),
`AUTH_RS_NATIVE_CONTAINER` (default `datalens-native-auth-1`, key source in dev).

## Known (non-semantic) deviations

- JWT header JSON key order differs (`{"typ":"JWT","alg":"PS256"}` vs `{"alg":…,"typ":…}`);
  every consumer parses JSON, and tokens are byte-different anyway because `iat` and the PSS
  salt are unique. Claims, `alg`, signature verification and TTLs match.
- Signup validation reproduces the common zod issue shapes exactly (checked live); exotic
  invalid bodies can differ in zod `details` wording while keeping status/code/message.
- Without an `x-forwarded-for` header the recorded `user_ip` is the real TCP peer. The Node
  container, reached through Docker's published port, records the Docker gateway IP instead —
  in the real cutover the UI server forwards `x-forwarded-for` (as it does for refresh), and
  then both services record the same value.
- No `pool` route, no static file serving, no compression middleware — none are used by the
  auth protocol clients.

## Security notes

- The PS256 private key is the instance secret: it is read from `TOKEN_PRIVATE_KEY`, never
  logged, and never written to disk by this service.
- Password hashes are verified with constant-time comparison; hashing/verification runs in
  `spawn_blocking` so scrypt does not stall the async runtime.
- Session/logout semantics are preserved: only the *current* refresh token can delete a
  session (a rotated token is rejected), and a refresh token presented from another IP for a
  known session deletes that session, exactly like the Node service.
- Access tokens are validated with the same rules as Node `jsonwebtoken.verify`
  (`PS256` only, `exp` checked when present, zero clock leeway) — an invalid/expired token is
  a `401` on the profile route.
- `pg-auth-db` is reached with a plaintext loopback connection (`tokio-postgres` `NoTls`).
  For a managed/remote PostgreSQL, terminate TLS via a local tunnel or add
  `tokio-postgres-rustls`; do not expose the DSN over an untrusted network.
- No `/private/*` master-token routes are exposed, so the service cannot be used as an
  administrative back door.

## Cutover checklist (Lead)

1. Start `datalens-auth-rs` with the production keys/DSN; check `/ping`, `/ping-db`.
2. Run `python3 tests/parity_live.py` while the Node container still runs.
3. Point mono-ui's `AUTH_ENDPOINT` at the Rust service and sign in / refresh / log out / load
   a dashboard; keep the Node container stopped but present for one release as a fallback.
4. Roll back by restoring `AUTH_ENDPOINT` — sessions created by Rust remain valid for Node
   because both share `pg-auth-db` and the same keys.
