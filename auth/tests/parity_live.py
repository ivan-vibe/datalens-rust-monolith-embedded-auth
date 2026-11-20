#!/usr/bin/env python3
"""Live differential tests: `datalens-auth-rs` vs the reference DataLens auth service.

The reference is the still-running `datalens-python-auth-1` container (same image as the
native one, `datalens-auth:0.27.0`) on 127.0.0.1:28187. The Rust service is started by this
script on 127.0.0.1:8097 with the *same* PS256 keys and the *same* `pg-auth-db`, so both
services see one database — which is exactly the cutover topology.

Covered:
  * static routes (/ping, /ping-db, /ping-db-primary, /, /signin-fail, 404s)
  * signin: status/body/cookie structure, JWT header+claims, DB session rows
  * profile cross-acceptance: Node token → Rust and Rust token → Node
  * refresh token interop in both directions, rotation, stale-token rejection
  * logout: response, cookie clearing, DB cascade, stale-cookie rejection
  * signup: validation-error parity, success parity (scratch users), duplicate 409,
    cleanup of all scratch users/sessions
  * signup-disabled behaviour (second instance with AUTH_SIGNUP_DISABLED=true)
  * user management: /v1/users/list + get-by-ids (paging, filters, escaping, validation),
    me profile/password, management profile/password/create/delete, roles add/update/remove,
    permission failures (viewer/role-less), IdP-user guards, private master-token variants,
    and the 404 behaviour for wrong methods

Usage:
    cargo build --release            # once
    python3 tests/parity_live.py     # requires docker + the reference container

Env overrides: AUTH_RS_REF, AUTH_RS_TEST_PORT, AUTH_RS_DISABLED_PORT,
AUTH_RS_PG_CONTAINER, AUTH_RS_KEEP_DATA=1 (skip cleanup).
Exit code 0 only when every check passes; failures print the concrete diff.
"""

import base64
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REF = os.environ.get("AUTH_RS_REF", "http://127.0.0.1:28187").rstrip("/")


def pick_port(preferred, label):
    """Use the configured port, or a free ephemeral one when it is already taken (e.g. a
    developer's running instance)."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", preferred))
            return preferred
    except OSError:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            chosen = probe.getsockname()[1]
        print(f"port {preferred} is busy; using :{chosen} for the {label} instance")
        return chosen


RUST_PORT = pick_port(int(os.environ.get("AUTH_RS_TEST_PORT", "8097")), "test")
RUST = f"http://127.0.0.1:{RUST_PORT}"
DISABLED_PORT = pick_port(int(os.environ.get("AUTH_RS_DISABLED_PORT", "8098")), "signup-disabled")
PG_CONTAINER = os.environ.get("AUTH_RS_PG_CONTAINER", "datalens-python-postgres-1")
NATIVE_AUTH_CONTAINER = os.environ.get("AUTH_RS_NATIVE_CONTAINER", "datalens-native-auth-1")
DSN = os.environ.get(
    "AUTH_POSTGRES_DSN",
    "postgres://pg-user:postgres@127.0.0.1:28185/pg-auth-db",
)
LOGIN = "admin"
PASSWORD = "admin"
TEST_UA = "auth-rs-parity/1.0"
XFF = "203.0.113.9"
MASTER_TOKEN = os.environ.get("AUTH_MASTER_TOKEN", "auth-master-token")
ACCESS_TTL = 900
REFRESH_TTL = 60 * 60 * 24 * 10
SESSION_TTL = 60 * 60 * 24 * 30

checks = []
failures = []
created_sessions = set()
created_users = set()
procs = []


# ------------------------------------------------------------------------------------------
# tiny HTTP client (no redirect following: signin failure is a 302 we must compare)
# ------------------------------------------------------------------------------------------


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_OPENER = urllib.request.build_opener(_NoRedirect)


class Response:
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = headers
        self.body = body

    def header(self, name):
        return self.headers.get(name)

    def header_all(self, name):
        return self.headers.get_all(name) or []

    def text(self):
        return self.body.decode("utf-8", "replace")

    def json(self):
        return json.loads(self.body)


def http(base, method, path, body=None, headers=None, timeout=30):
    send = None
    request_headers = {"User-Agent": TEST_UA, "X-Forwarded-For": XFF}
    if body is not None:
        send = json.dumps(body).encode()
        request_headers["Content-Type"] = "application/json"
    if headers:
        request_headers.update(headers)
    request = urllib.request.Request(base + path, data=send, headers=request_headers, method=method)
    try:
        with _OPENER.open(request, timeout=timeout) as response:
            return Response(response.status, response.headers, response.read())
    except urllib.error.HTTPError as error:
        return Response(error.code, error.headers, error.read())


# ------------------------------------------------------------------------------------------
# assertions
# ------------------------------------------------------------------------------------------


def check(name, ok, detail=""):
    checks.append((name, ok, detail))
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f"\n      {detail}"))
    if not ok:
        failures.append(name)


def eq(name, reference, rust, extra=""):
    ok = reference == rust
    detail = "" if ok else f"reference={reference!r}\n      rust={rust!r}" + (f"\n      {extra}" if extra else "")
    check(name, ok, detail)


def eq_json(name, reference_response, rust_response):
    same_status = reference_response.status == rust_response.status
    try:
        reference_json = reference_response.json()
        rust_json = rust_response.json()
    except Exception:  # noqa: BLE001 - report body, not a crash
        eq(name, (reference_response.status, reference_response.text()),
           (rust_response.status, rust_response.text()))
        return
    ok = same_status and reference_json == rust_json
    detail = ""
    if not ok:
        detail = (
            f"status reference={reference_response.status} rust={rust_response.status}\n"
            f"      body reference={json.dumps(reference_json, ensure_ascii=False)}\n"
            f"      body rust={json.dumps(rust_json, ensure_ascii=False)}"
        )
    check(name, ok, detail)


# ------------------------------------------------------------------------------------------
# cookies / JWT / ids
# ------------------------------------------------------------------------------------------


def set_cookie_lines(response):
    lines = response.header_all("Set-Cookie")
    # Multiple headers may also arrive folded with commas only in odd servers; the reference
    # and axum both send separate headers, so get_all is enough here.
    return lines


def cookie_map(lines):
    out = {}
    for line in lines:
        pair = line.split(";", 1)[0]
        name, _, value = pair.partition("=")
        out[name.strip()] = value
    return out


def cookie_header(lines):
    return "; ".join(f"{name}={value}" for name, value in cookie_map(lines).items())


def normalized_cookie_attributes(line):
    parts = [part.strip() for part in line.split(";")]
    result = []
    for index, part in enumerate(parts):
        if index == 0:
            # Cookie *values* legitimately differ (tokens are unique); only the attributes
            # are part of the protocol contract.
            name = part.partition("=")[0]
            result.append(name + "=<value>")
        elif part.lower().startswith("expires="):
            result.append("Expires=<epoch>")
        else:
            result.append(part)
    return result


def compare_set_cookie_structure(name, reference_lines, rust_lines):
    eq(name + " count", len(reference_lines), len(rust_lines))
    eq(name + " names/attrs",
       [normalized_cookie_attributes(line) for line in reference_lines],
       [normalized_cookie_attributes(line) for line in rust_lines])


def b64url_decode(part):
    return base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))


def jwt_parts(token):
    header_b64, payload_b64, _signature = token.split(".")
    return json.loads(b64url_decode(header_b64)), json.loads(b64url_decode(payload_b64))


def auth_cookie_json(lines):
    # The `auth` cookie value is URL-encoded JSON (encodeURIComponent), as the browser sends it.
    return json.loads(urllib.parse.unquote(cookie_map(lines)["auth"]))


def access_token(lines):
    return auth_cookie_json(lines)["accessToken"]


def refresh_token(lines):
    return auth_cookie_json(lines)["refreshToken"]


CODING_BASE = "0123456789abcdefghijklmnopqrstuvwxyz"


def decode_id(encoded):
    rotation = CODING_BASE.index(encoded[-1])
    base = CODING_BASE[rotation:] + CODING_BASE[:rotation]
    value = 0
    for char in encoded[:-1]:
        value = value * 36 + base.index(char)
    return value


def encode_id(value):
    if value == 0:
        return ""
    rotation = (value % 100) % 36
    base = CODING_BASE[rotation:] + CODING_BASE[:rotation]
    digits = []
    while value > 0:
        digits.append(base[value % 36])
        value //= 36
    return "".join(reversed(digits)) + CODING_BASE[rotation]


def signin_token(base, login, password):
    """Sign in and return the access token; the created session is cleaned up afterwards."""
    response = http(base, "POST", "/signin", {"login": login, "password": password})
    if response.status != 200:
        raise RuntimeError(f"signin {login} on {base} failed: {response.status} {response.text()}")
    token = access_token(set_cookie_lines(response))
    created_sessions.add(decode_id(jwt_parts(token)[1]["sessionId"]))
    return token


def admin_token():
    return signin_token(REF, LOGIN, PASSWORD)


def with_token(token, extra=None):
    headers = {"Authorization": "Bearer " + token}
    if extra:
        headers.update(extra)
    return headers


def compare(name, method, path, body=None, headers_ref=None, headers_rust=None):
    reference = http(REF, method, path, body, headers=headers_ref)
    rust = http(RUST, method, path, body, headers=headers_rust)
    eq_json(name, reference, rust)
    return reference, rust


def query_value(value):
    return urllib.parse.quote(str(value), safe='')


# ------------------------------------------------------------------------------------------
# database (read/cleanup only; db writes happen through the services)
# ------------------------------------------------------------------------------------------


def psql(sql):
    result = subprocess.run(
        ["docker", "exec", PG_CONTAINER, "psql", "-U", "pg-user", "-d", "pg-auth-db", "-tAc", sql],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(f"psql failed: {result.stderr.strip()}\nSQL: {sql}")
    return result.stdout.strip()


def session_row(session_id):
    user_agent = psql(
        f"SELECT user_agent FROM auth_sessions WHERE session_id = {session_id}"
    )
    user_ip = psql(f"SELECT user_ip FROM auth_sessions WHERE session_id = {session_id}")
    session_lifetime = psql(
        f"SELECT (EXTRACT(EPOCH FROM (expired_at - created_at)))::bigint "
        f"FROM auth_sessions WHERE session_id = {session_id}"
    )
    token_count = psql(
        f"SELECT count(*) FROM auth_refresh_tokens WHERE session_id = {session_id}"
    )
    token_lifetime = psql(
        f"SELECT (EXTRACT(EPOCH FROM (rt.expired_at - rt.created_at)))::bigint "
        f"FROM auth_refresh_tokens rt WHERE rt.session_id = {session_id} LIMIT 1"
    )
    token_id = psql(
        f"SELECT refresh_token_id FROM auth_refresh_tokens WHERE session_id = {session_id} LIMIT 1"
    )
    return {
        "user_agent": user_agent,
        "user_ip": user_ip,
        "session_lifetime": session_lifetime,
        "token_count": token_count,
        "token_lifetime": token_lifetime,
        "refresh_token_id": token_id,
    }


def assert_session_rows(prefix, lines):
    header, claims = jwt_parts(access_token(lines))
    session_id = decode_id(claims["sessionId"])
    created_sessions.add(session_id)
    check(prefix + " jwt header alg/typ", header.get("alg") == "PS256" and header.get("typ") == "JWT",
          f"header={header}")
    expected_claims = {"userId", "sessionId", "roles", "iat", "exp"}
    eq(prefix + " access claim keys", expected_claims, set(claims))
    eq(prefix + " access ttl", ACCESS_TTL, claims["exp"] - claims["iat"])
    refresh_header, refresh_claims = jwt_parts(refresh_token(lines))
    eq(prefix + " refresh token ttl field absent", False, "exp" in refresh_claims)
    eq(prefix + " refresh claim keys",
       {"refreshTokenId", "userId", "sessionId", "iat"}, set(refresh_claims))
    eq(prefix + " refresh header alg", "PS256", refresh_header.get("alg"))

    row = session_row(session_id)
    eq(prefix + " db user_agent", TEST_UA, row["user_agent"])
    eq(prefix + " db user_ip", XFF, row["user_ip"])
    eq(prefix + " db session lifetime", str(SESSION_TTL), row["session_lifetime"])
    eq(prefix + " db one refresh token", "1", row["token_count"])
    eq(prefix + " db refresh lifetime", str(REFRESH_TTL), row["token_lifetime"])
    eq(prefix + " db refresh id matches claim",
       row["refresh_token_id"], str(decode_id(refresh_claims["refreshTokenId"])))
    return claims, refresh_claims


# ------------------------------------------------------------------------------------------
# service lifecycle
# ------------------------------------------------------------------------------------------


def container_env(container, name):
    result = subprocess.run(
        ["docker", "inspect", "--format", "{{range .Config.Env}}{{println .}}{{end}}", container],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        if line.startswith(name + "="):
            return line[len(name) + 1:]
    return None


def wait_for(base, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if http(base, "GET", "/ping", timeout=1).status == 200:
                return True
        except Exception:  # noqa: BLE001
            pass
        time.sleep(0.2)
    return False


def start_rust(port, extra_env):
    binary = ROOT / "target" / "release" / "datalens-auth-rs"
    if not binary.exists():
        print("building release binary (cargo build --release) ...")
        build = subprocess.run(["cargo", "build", "--release"], cwd=ROOT)
        if build.returncode != 0:
            raise SystemExit("cargo build --release failed")
    env = os.environ.copy()
    env["AUTH_PORT"] = str(port)
    env["AUTH_POSTGRES_DSN"] = DSN
    if not env.get("TOKEN_PRIVATE_KEY"):
        env["TOKEN_PRIVATE_KEY"] = container_env(NATIVE_AUTH_CONTAINER, "TOKEN_PRIVATE_KEY") or ""
    if not env.get("TOKEN_PUBLIC_KEY"):
        env["TOKEN_PUBLIC_KEY"] = container_env(NATIVE_AUTH_CONTAINER, "TOKEN_PUBLIC_KEY") or ""
    if not env["TOKEN_PRIVATE_KEY"] or not env["TOKEN_PUBLIC_KEY"]:
        raise SystemExit(
            "TOKEN_PRIVATE_KEY/TOKEN_PUBLIC_KEY are not set and could not be read from "
            f"{NATIVE_AUTH_CONTAINER}; export them or start the reference container"
        )
    env.update(extra_env)
    log_path = Path(tempfile.gettempdir()) / f"auth-rs-parity-{port}.log"
    log = open(log_path, "w", encoding="utf-8")
    proc = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
    procs.append(proc)
    if not wait_for(f"http://127.0.0.1:{port}"):
        log.close()
        print(log_path.read_text(encoding="utf-8"))
        raise SystemExit(f"datalens-auth-rs on :{port} did not become ready")
    return proc


def cleanup():
    if os.environ.get("AUTH_RS_KEEP_DATA") == "1":
        return
    if created_sessions:
        ids = ", ".join(str(value) for value in sorted(created_sessions))
        try:
            psql(f"DELETE FROM auth_sessions WHERE session_id IN ({ids})")
        except Exception as error:  # noqa: BLE001
            print(f"cleanup warning (sessions): {error}")
    if created_users:
        logins = ", ".join(f"'{login}'" for login in sorted(created_users))
        try:
            psql(f"DELETE FROM auth_users WHERE login IN ({logins})")
        except Exception as error:  # noqa: BLE001
            print(f"cleanup warning (users): {error}")


# ------------------------------------------------------------------------------------------
# test groups
# ------------------------------------------------------------------------------------------


def test_static_routes():
    for path in ["/ping", "/ping-db", "/ping-db-primary", "/", "/signin-fail"]:
        eq_json(f"GET {path}", http(REF, "GET", path), http(RUST, "GET", path))

    reference_ping = http(REF, "GET", "/ping")
    rust_ping = http(RUST, "GET", "/ping")
    eq("GET /ping cache-control", reference_ping.header("cache-control"),
       rust_ping.header("cache-control"))
    eq("GET /ping surrogate-control", reference_ping.header("surrogate-control"),
       rust_ping.header("surrogate-control"))
    eq("GET /ping content-type", reference_ping.header("content-type"),
       rust_ping.header("content-type"))

    eq("GET /signup 404", (http(REF, "GET", "/signup").status, http(REF, "GET", "/signup").text()),
       (http(RUST, "GET", "/signup").status, http(RUST, "GET", "/signup").text()))
    reference_404 = http(REF, "GET", "/signup")
    rust_404 = http(RUST, "GET", "/signup")
    eq("404 content-type", reference_404.header("content-type"), rust_404.header("content-type"))
    eq("404 csp", reference_404.header("content-security-policy"),
       rust_404.header("content-security-policy"))
    eq("404 nosniff", reference_404.header("x-content-type-options"),
       rust_404.header("x-content-type-options"))

    malformed = ("POST", "/signin", "{bad", {"Content-Type": "application/json"})
    def raw(base, method, path, payload, headers):
        request = urllib.request.Request(base + path, data=payload.encode(),
                                         headers=headers, method=method)
        try:
            with _OPENER.open(request, timeout=10) as response:
                return Response(response.status, response.headers, response.read())
        except urllib.error.HTTPError as error:
            return Response(error.code, error.headers, error.read())

    reference_malformed = raw(REF, *malformed)
    rust_malformed = raw(RUST, *malformed)
    eq("malformed JSON status", reference_malformed.status, rust_malformed.status)
    eq("malformed JSON body", reference_malformed.text(), rust_malformed.text())
    eq("malformed JSON content-type", reference_malformed.header("content-type"),
       rust_malformed.header("content-type"))

    no_content_type = raw(REF, "POST", "/signin", json.dumps({"login": LOGIN, "password": PASSWORD}),
                          {"User-Agent": TEST_UA})
    rust_no_content_type = raw(RUST, "POST", "/signin", json.dumps({"login": LOGIN, "password": PASSWORD}),
                               {"User-Agent": TEST_UA})
    eq("signin without content-type status", no_content_type.status, rust_no_content_type.status)
    eq("signin without content-type location",
       no_content_type.header("location"), rust_no_content_type.header("location"))

    bad_login_body = {"login": LOGIN, "password": "wrong-password"}
    reference_bad = http(REF, "POST", "/signin", bad_login_body)
    rust_bad = http(RUST, "POST", "/signin", bad_login_body)
    eq("bad signin status", reference_bad.status, rust_bad.status)
    eq("bad signin location", reference_bad.header("location"), rust_bad.header("location"))
    eq("bad signin body", reference_bad.text(), rust_bad.text())
    eq("bad signin vary", reference_bad.header("vary"), rust_bad.header("vary"))

    eq_json("refresh without cookie", http(REF, "POST", "/refresh"), http(RUST, "POST", "/refresh"))
    garbage = {"Cookie": "auth=not-json"}
    eq_json("refresh with garbage cookie", http(REF, "POST", "/refresh", headers=garbage),
            http(RUST, "POST", "/refresh", headers=garbage))

    reference_logout = http(REF, "GET", "/logout")
    rust_logout = http(RUST, "GET", "/logout")
    eq_json("logout without cookie body", reference_logout, rust_logout)
    eq("logout without cookie set-cookie", set_cookie_lines(reference_logout),
       set_cookie_lines(rust_logout))


def test_signin_and_profile():
    reference_signin = http(REF, "POST", "/signin", {"login": LOGIN, "password": PASSWORD})
    rust_signin = http(RUST, "POST", "/signin", {"login": LOGIN, "password": PASSWORD})
    eq("signin status", reference_signin.status, rust_signin.status)
    eq("signin body", reference_signin.text(), rust_signin.text())
    compare_set_cookie_structure("signin set-cookie",
                                 set_cookie_lines(reference_signin),
                                 set_cookie_lines(rust_signin))

    reference_claims, _ = assert_session_rows("reference signin", set_cookie_lines(reference_signin))
    rust_claims, _ = assert_session_rows("rust signin", set_cookie_lines(rust_signin))
    eq("signin userId claim", reference_claims["userId"], rust_claims["userId"])
    eq("signin roles claim", reference_claims["roles"], rust_claims["roles"])

    # Cross-acceptance: the whole point of the shared key + DB.
    for token_source, token_lines in [("reference", set_cookie_lines(reference_signin)),
                                      ("rust", set_cookie_lines(rust_signin))]:
        token = access_token(token_lines)
        headers = {"Authorization": "Bearer " + token}
        reference_profile = http(REF, "GET", "/v1/users/me/profile", headers=headers)
        rust_profile = http(RUST, "GET", "/v1/users/me/profile", headers=headers)
        eq(f"{token_source} token profile status (reference)", 200, reference_profile.status)
        eq(f"{token_source} token profile status (rust)", 200, rust_profile.status)
        eq_json(f"{token_source} token profile body", reference_profile, rust_profile)

    profile_body = http(RUST, "GET", "/v1/users/me/profile",
                        headers={"Authorization": "Bearer " + access_token(set_cookie_lines(rust_signin))}).json()
    eq("profile shape", {"profile": {"userId": rust_claims["userId"], "login": LOGIN,
                                     "email": None, "firstName": "Admin", "lastName": None,
                                     "idpType": None, "idpSlug": None,
                                     "roles": rust_claims["roles"]}}, profile_body)

    # Invalid/expired/absent tokens answer exactly like the reference.
    for label, headers in [
        ("no token", {}),
        ("garbage token", {"Authorization": "Bearer not.a.jwt"}),
        ("wrong scheme", {"Authorization": "Token " + access_token(set_cookie_lines(rust_signin))}),
    ]:
        eq(f"profile {label}", (http(REF, "GET", "/v1/users/me/profile", headers=headers).status,
                                http(REF, "GET", "/v1/users/me/profile", headers=headers).text()),
           (http(RUST, "GET", "/v1/users/me/profile", headers=headers).status,
            http(RUST, "GET", "/v1/users/me/profile", headers=headers).text()))
    return reference_signin, rust_signin


def test_refresh_interop(reference_signin, rust_signin):
    rust_cookie = {"Cookie": cookie_header(set_cookie_lines(rust_signin))}
    reference_cookie = {"Cookie": cookie_header(set_cookie_lines(reference_signin))}

    # Rust-issued cookie refreshed by the reference service (same key + DB).
    rust_rotated_by_reference = http(REF, "POST", "/refresh", headers=rust_cookie)
    eq("rust cookie → reference refresh status", 200, rust_rotated_by_reference.status)
    eq("rust cookie → reference refresh body", '{"done":true}', rust_rotated_by_reference.text())
    compare_set_cookie_structure("reference-refreshed cookie",
                                 set_cookie_lines(rust_rotated_by_reference),
                                 set_cookie_lines(rust_rotated_by_reference))
    original = jwt_parts(access_token(set_cookie_lines(rust_signin)))[1]
    rotated = jwt_parts(access_token(set_cookie_lines(rust_rotated_by_reference)))[1]
    eq("refresh keeps session", original["sessionId"], rotated["sessionId"])
    eq("refresh rotates refresh token",
       False, refresh_token(set_cookie_lines(rust_signin)) ==
       refresh_token(set_cookie_lines(rust_rotated_by_reference)))

    # ... and the reference-rotated cookie is accepted back by Rust.
    rust_accepts = http(RUST, "POST", "/refresh",
                        headers={"Cookie": cookie_header(set_cookie_lines(rust_rotated_by_reference))})
    eq("reference-rotated cookie → rust refresh status", 200, rust_accepts.status)
    eq("reference-rotated cookie → rust refresh body", '{"done":true}', rust_accepts.text())

    # Opposite direction: reference signin cookie refreshed by Rust.
    reference_rotated_by_rust = http(RUST, "POST", "/refresh", headers=reference_cookie)
    eq("reference cookie → rust refresh status", 200, reference_rotated_by_rust.status)
    eq("reference cookie → rust refresh body", '{"done":true}', reference_rotated_by_rust.text())
    reference_accepts = http(REF, "POST", "/refresh",
                             headers={"Cookie": cookie_header(set_cookie_lines(reference_rotated_by_rust))})
    eq("rust-rotated cookie → reference refresh status", 200, reference_accepts.status)

    # A rotated (stale) refresh token is rejected by both, with the same body.
    stale = {"Cookie": cookie_header(set_cookie_lines(rust_signin))}
    eq("stale token reference refresh", http(REF, "POST", "/refresh", headers=stale).status, 401)
    eq_json("stale token rejection", http(REF, "POST", "/refresh", headers=stale),
            http(RUST, "POST", "/refresh", headers=stale))

    # Access tokens minted by a refresh work on both profile endpoints.
    fresh = access_token(set_cookie_lines(rust_accepts))
    headers = {"Authorization": "Bearer " + fresh}
    eq("refreshed token profile (reference)", 200,
       http(REF, "GET", "/v1/users/me/profile", headers=headers).status)
    eq("refreshed token profile (rust)", 200,
       http(RUST, "GET", "/v1/users/me/profile", headers=headers).status)

    # The DB holds exactly one refresh-token row per session after rotations.
    session_id = decode_id(rotated["sessionId"])
    created_sessions.add(session_id)
    row = session_row(session_id)
    eq("rotated session db token count", "1", row["token_count"])
    eq("rotated session db ip", XFF, row["user_ip"])


def test_logout():
    rust_signin = http(RUST, "POST", "/signin", {"login": LOGIN, "password": PASSWORD})
    lines = set_cookie_lines(rust_signin)
    claims = jwt_parts(access_token(lines))[1]
    session_id = decode_id(claims["sessionId"])
    created_sessions.add(session_id)
    cookie = {"Cookie": cookie_header(lines)}

    rust_logout = http(RUST, "GET", "/logout", headers=cookie)
    eq("rust logout status", 200, rust_logout.status)
    eq("rust logout body", '{"done":true}', rust_logout.text())
    eq("rust logout clears cookies", set_cookie_lines(http(REF, "GET", "/logout")),
       set_cookie_lines(rust_logout))
    eq("logout deletes session", "0",
       psql(f"SELECT count(*) FROM auth_sessions WHERE session_id = {session_id}"))
    eq("logout cascades refresh token", "0",
       psql(f"SELECT count(*) FROM auth_refresh_tokens WHERE session_id = {session_id}"))

    for base, label in [(REF, "reference"), (RUST, "rust")]:
        response = http(base, "POST", "/refresh", headers=cookie)
        eq(f"refresh after logout ({label})", 401, response.status)
        eq(f"refresh after logout body ({label})",
           '{"code":"AUTH.NEED_RESET","message":"Can\'t refresh tokens"}', response.text())

    # Same behaviour through the reference service.
    reference_signin = http(REF, "POST", "/signin", {"login": LOGIN, "password": PASSWORD})
    reference_claims = jwt_parts(access_token(set_cookie_lines(reference_signin)))[1]
    reference_session = decode_id(reference_claims["sessionId"])
    created_sessions.add(reference_session)
    reference_logout = http(REF, "GET", "/logout",
                            headers={"Cookie": cookie_header(set_cookie_lines(reference_signin))})
    eq("reference logout status", 200, reference_logout.status)
    eq("reference logout deletes session", "0",
       psql(f"SELECT count(*) FROM auth_sessions WHERE session_id = {reference_session}"))


def normalized_validation_details(response):
    try:
        body = response.json()
    except Exception:  # noqa: BLE001
        return ("non-json", response.text())
    details = body.get("details")
    if isinstance(details, list):
        details = sorted(json.dumps(item, sort_keys=True) for item in details)
    return body.get("code"), body.get("message"), details


def test_signup_enabled():
    for label, body in [
        ("weak password", {"login": "x", "password": "y"}),
        ("missing fields", {}),
        ("array body", []),
        ("invalid email", {"login": "user@bad", "password": "Abcdef1!"}),
    ]:
        reference = http(REF, "POST", "/signup", body)
        rust = http(RUST, "POST", "/signup", body)
        eq(f"signup validation status ({label})", reference.status, rust.status)
        eq(f"signup validation payload ({label})",
           normalized_validation_details(reference), normalized_validation_details(rust))
        eq(f"signup validation content-type ({label})",
           reference.header("content-type"), rust.header("content-type"))

    reference_login = "authrstestref" + uuid.uuid4().hex[:8]
    rust_login = "authrstestrust" + uuid.uuid4().hex[:8]
    created_users.update({reference_login, rust_login})
    password = "Abcdef1!"

    reference_signup = http(REF, "POST", "/signup",
                            {"login": reference_login, "password": password,
                             "email": reference_login + "@example.com",
                             "firstName": "Ref", "lastName": "User"})
    rust_signup = http(RUST, "POST", "/signup",
                       {"login": rust_login, "password": password,
                        "email": rust_login + "@example.com",
                        "firstName": "Rust", "lastName": "User"})
    eq("signup status (reference)", 200, reference_signup.status)
    eq("signup status (rust)", 200, rust_signup.status)
    eq("signup body", reference_signup.text(), rust_signup.text())
    compare_set_cookie_structure("signup set-cookie",
                                 set_cookie_lines(reference_signup),
                                 set_cookie_lines(rust_signup))

    reference_claims = jwt_parts(access_token(set_cookie_lines(reference_signup)))[1]
    rust_claims = jwt_parts(access_token(set_cookie_lines(rust_signup)))[1]
    created_sessions.add(decode_id(reference_claims["sessionId"]))
    created_sessions.add(decode_id(rust_claims["sessionId"]))
    eq("signup default role", ["datalens.viewer"], rust_claims["roles"])
    eq("signup roles parity", reference_claims["roles"], rust_claims["roles"])

    eq("signup user row (reference)", reference_login,
       psql(f"SELECT login FROM auth_users WHERE login = '{reference_login}'"))
    eq("signup user row (rust)", rust_login,
       psql(f"SELECT login FROM auth_users WHERE login = '{rust_login}'"))
    eq("signup role row (rust)", "datalens.viewer",
       psql(f"SELECT role FROM auth_roles WHERE user_id = "
            f"(SELECT user_id FROM auth_users WHERE login = '{rust_login}')"))
    eq("signup password is scrypt-hashed", "1",
       psql(f"SELECT count(*) FROM auth_users WHERE login = '{rust_login}' "
            f"AND password LIKE '%:%' AND password NOT LIKE '%{password}%'"))

    rust_headers = {"Authorization": "Bearer " + access_token(set_cookie_lines(rust_signup))}
    reference_headers = {"Authorization": "Bearer " + access_token(set_cookie_lines(reference_signup))}
    eq("signup token works on rust profile", 200,
       http(RUST, "GET", "/v1/users/me/profile", headers=rust_headers).status)
    eq("signup token works on reference profile", 200,
       http(REF, "GET", "/v1/users/me/profile", headers=rust_headers).status)
    expected_rust_profile = {"profile": {
        "userId": rust_claims["userId"], "login": rust_login,
        "email": rust_login + "@example.com", "firstName": "Rust", "lastName": "User",
        "idpType": None, "idpSlug": None, "roles": ["datalens.viewer"]}}
    eq("signup profile shape",
       expected_rust_profile, http(RUST, "GET", "/v1/users/me/profile", headers=rust_headers).json())
    eq_json("signup profile cross-service",
            http(REF, "GET", "/v1/users/me/profile", headers=rust_headers),
            http(RUST, "GET", "/v1/users/me/profile", headers=rust_headers))
    eq_json("signup reference profile cross-service",
            http(REF, "GET", "/v1/users/me/profile", headers=reference_headers),
            http(RUST, "GET", "/v1/users/me/profile", headers=reference_headers))

    duplicate = http(RUST, "POST", "/signup", {"login": rust_login, "password": password})
    eq("duplicate signup status", 409, duplicate.status)
    eq("duplicate signup body",
       '{"code":"AUTH.USER_ALREADY_EXISTS","message":"The user already exists"}', duplicate.text())
    duplicate_reference = http(REF, "POST", "/signup", {"login": reference_login, "password": password})
    eq("duplicate signup parity", (duplicate.status, duplicate.body),
       (duplicate_reference.status, duplicate_reference.body))


def test_signup_disabled(disabled_base):
    known = ('{"message":"Signup is disabled","code":"AUTH.SIGNUP_DISABLED"}')
    response = http(disabled_base, "POST", "/signup", {"login": "someone", "password": "Abcdef1!"})
    eq("disabled signup status", 403, response.status)
    eq("disabled signup body", known, response.text())
    eq("disabled signup does not create users", "0",
       psql("SELECT count(*) FROM auth_users WHERE login = 'someone'"))
    eq("GET /signup is 404", 404, http(disabled_base, "GET", "/signup").status)


def test_user_management():
    """Public/private user-management parity, using only generated `authrstest-*` users."""
    strong = "Abcdef1!"
    admin = admin_token()
    admin_headers = with_token(admin)
    viewer_headers = None
    master_headers = {"x-dl-auth-master-token": MASTER_TOKEN}
    suffix = uuid.uuid4().hex[:8]

    def new_login(kind):
        login = f"authrstest-{kind}{suffix}{uuid.uuid4().hex[:4]}"
        created_users.add(login)
        return login

    # --- create users through each service ------------------------------------------------
    ref_login = new_login("ref")
    rust_login = new_login("rust")
    ref_create = http(
        REF, "POST", "/v1/management/users/create",
        {"login": ref_login, "password": strong, "email": ref_login + "@example.com",
         "firstName": "RefFirst", "lastName": "RefLast"},
        headers=admin_headers,
    )
    rust_create = http(
        RUST, "POST", "/v1/management/users/create",
        {"login": rust_login, "password": strong, "email": rust_login + "@example.com",
         "firstName": "RustFirst", "lastName": "RustLast"},
        headers=admin_headers,
    )
    eq("management create status (reference)", 200, ref_create.status)
    eq("management create status (rust service)", 200, rust_create.status)
    ref_id = ref_create.json().get("userId")
    rust_id = rust_create.json().get("userId")
    eq("management create shapes", set(ref_create.json()), set(rust_create.json()))
    eq("management create row (reference)", ref_login,
       psql(f"SELECT login FROM auth_users WHERE user_id = {decode_id(ref_id)}"))
    eq("management create role (rust service)", "datalens.viewer",
       psql(f"SELECT role FROM auth_roles WHERE user_id = {decode_id(rust_id)}"))
    eq("management create password is hashed", "1",
       psql(f"SELECT count(*) FROM auth_users WHERE user_id = {decode_id(rust_id)} "
            f"AND password LIKE '%:%' AND password NOT LIKE '%{strong}%'"))

    # extra scratch user with "_" in firstName for LIKE escaping checks
    escape_login = new_login("esc")
    escape_create = http(
        REF, "POST", "/v1/management/users/create",
        {"login": escape_login, "password": strong, "firstName": "Under_Score"},
        headers=admin_headers,
    )
    eq("management create escaping user", 200, escape_create.status)

    # --- GET /v1/users/list ---------------------------------------------------------------
    list_paths = [
        f"/v1/users/list?filterString={query_value(ref_login)}",
        f"/v1/users/list?filterString={query_value(rust_login)}",
        f"/v1/users/list?filterString={query_value(ref_login[4:12])}",
        f"/v1/users/list?filterString={query_value(ref_login + '@example.com')}",
        f"/v1/users/list?filterString={query_value('reffirst')}",
        "/v1/users/list?pageSize=1&filterString=authrstest-",
        "/v1/users/list?page=999&pageSize=5",
        "/v1/users/list?roles=datalens.admin&pageSize=2",
        "/v1/users/list?idpType=null&pageSize=1",
        "/v1/users/list?filterString=_",
        "/v1/users/list?filterString=%25",
        "/v1/users/list?page=x",
        "/v1/users/list?page=-1",
        "/v1/users/list?pageSize=0",
        "/v1/users/list?pageSize=101",
        "/v1/users/list?filterString=",
        "/v1/users/list?roles=bogus",
        "/v1/users/list?roles=bogus&pageSize=2",
        "/v1/users/list?roles=datalens.admin&roles=datalens.viewer&pageSize=1",
        "/v1/users/list",
    ]
    for path in list_paths:
        compare(f"GET {path}", "GET", path, headers_ref=admin_headers, headers_rust=admin_headers)

    # --- POST /v1/users/get-by-ids ---------------------------------------------------------
    by_ids_cases = [
        {"subjectIds": [ref_id, rust_id]},
        {"subjectIds": [ref_id, ref_id]},
        {"subjectIds": ["123"]},
        {"subjectIds": ["12$"]},
        {"subjectIds": []},
        {},
        {"subjectIds": [123]},
        {"subjectIds": "not-an-array"},
        {},
    ]
    for body in by_ids_cases:
        compare(f"POST get-by-ids {json.dumps(body, sort_keys=True)}", "POST",
                "/v1/users/get-by-ids", body, admin_headers, admin_headers)
    compare("get-by-ids wrong method", "GET", "/v1/users/get-by-ids",
            headers_ref=admin_headers, headers_rust=admin_headers)

    # --- permission failures ---------------------------------------------------------------
    compare("list without token", "GET", "/v1/users/list")
    compare("management create without token", "POST", "/v1/management/users/create",
            {"login": "authrstest-noauth", "password": strong})

    viewer_login = new_login("view")
    viewer_signup = http(REF, "POST", "/signup", {"login": viewer_login, "password": strong})
    eq("viewer signup", 200, viewer_signup.status)
    created_sessions.add(decode_id(jwt_parts(access_token(set_cookie_lines(viewer_signup)))[1]["sessionId"]))
    viewer = signin_token(REF, viewer_login, strong)
    viewer_headers = with_token(viewer)
    compare(f"viewer list", "GET",
            f"/v1/users/list?filterString={query_value(viewer_login)}",
            headers_ref=viewer_headers, headers_rust=viewer_headers)
    for label, method, path, body in [
        ("viewer management create", "POST", "/v1/management/users/create",
         {"login": "authrstest-viewer-nope" + suffix, "password": strong}),
        ("viewer management profile", "GET", f"/v1/management/users/{ref_id}/profile", None),
        ("viewer management profile update", "POST", f"/v1/management/users/{ref_id}/profile",
         {"firstName": "Nope"}),
        ("viewer management password", "POST", f"/v1/management/users/{ref_id}/password",
         {"newPassword": strong}),
        ("viewer management delete", "DELETE", f"/v1/management/users/{ref_id}", None),
        ("viewer roles add", "POST", "/v1/management/users/roles/add",
         {"deltas": [{"role": "datalens.editor", "subjectId": ref_id}]}),
    ]:
        compare(label, method, path, body, viewer_headers, viewer_headers)

    roleless_login = new_login("roleless")
    roleless_create = http(REF, "POST", "/v1/management/users/create",
                           {"login": roleless_login, "password": strong}, headers=admin_headers)
    roleless_id = roleless_create.json()["userId"]
    psql(f"DELETE FROM auth_roles WHERE user_id = {decode_id(roleless_id)}")
    roleless = signin_token(REF, roleless_login, strong)
    eq("role-less token has no roles", [], jwt_parts(roleless)[1]["roles"])
    compare("role-less user list", "GET",
            f"/v1/users/list?filterString={query_value(roleless_login)}",
            headers_ref=with_token(roleless), headers_rust=with_token(roleless))

    # --- GET /v1/management/users/:userId/profile -----------------------------------------
    for path in [
        f"/v1/management/users/{ref_id}/profile",
        f"/v1/management/users/{rust_id}/profile",
        "/v1/management/users/123/profile",
        "/v1/management/users/12$/profile",
    ]:
        compare(f"GET {path}", "GET", path, headers_ref=admin_headers, headers_rust=admin_headers)

    profile = http(REF, "GET", f"/v1/management/users/{rust_id}/profile", headers=admin_headers)
    eq("management profile shape", {
        "profile": {
            "userId": rust_id, "login": rust_login, "email": rust_login + "@example.com",
            "firstName": "RustFirst", "lastName": "RustLast", "idpType": None,
            "idpSlug": None, "roles": ["datalens.viewer"],
        }
    }, profile.json())

    # --- POST /v1/management/users/:userId/profile -----------------------------------------
    profile_updates = [
        {"firstName": "RefEdit", "email": None},
        {"lastName": "RustEdit"},
        {},
        {"email": "bad"},
        {"firstName": ""},
        {"firstName": None, "lastName": None, "email": None},
        [],
    ]
    for body in profile_updates:
        compare(f"management update profile {json.dumps(body)}", "POST",
                f"/v1/management/users/{ref_id}/profile", body, admin_headers, admin_headers)
        compare(f"management profile after {json.dumps(body)}", "GET",
                f"/v1/management/users/{ref_id}/profile",
                headers_ref=admin_headers, headers_rust=admin_headers)
    compare("management update missing user profile", "POST",
            "/v1/management/users/123/profile", {"firstName": "X"}, admin_headers, admin_headers)

    # --- POST /v1/management/users/:userId/password ---------------------------------------
    for body in [{"newPassword": "short"}, {}, {"newPassword": "Abcdef1!"}]:
        compare(f"management update password {json.dumps(body)}", "POST",
                f"/v1/management/users/{ref_id}/password", body, admin_headers, admin_headers)
    compare("management update missing user password", "POST",
            "/v1/management/users/123/password", {"newPassword": strong},
            admin_headers, admin_headers)
    new_password = "Zyxwvu9!"
    eq("management password changed (rust service)", 200,
       http(RUST, "POST", f"/v1/management/users/{rust_id}/password",
            {"newPassword": new_password}, headers=admin_headers).status)
    signin_token(REF, rust_login, new_password)  # cross-service password verification
    old_ref = http(REF, "POST", "/signin", {"login": rust_login, "password": strong})
    old_rust = http(RUST, "POST", "/signin", {"login": rust_login, "password": strong})
    eq("old password rejected (status)", old_ref.status, old_rust.status)
    eq("old password rejected (body)", old_ref.text(), old_rust.text())

    # --- roles add / update / remove -------------------------------------------------------
    for label, body in [
        ("roles add editor", {"deltas": [{"role": "datalens.editor", "subjectId": ref_id}]}),
        ("roles add duplicate", {"deltas": [{"role": "datalens.editor", "subjectId": ref_id}]}),
        ("roles add invalid enum", {"deltas": [{"role": "bogus", "subjectId": ref_id}]}),
        ("roles add invalid subject", {"deltas": [{"role": "datalens.editor", "subjectId": "12$"}]}),
        ("roles add unknown user", {"deltas": [{"role": "datalens.editor", "subjectId": "zz"}]}),
        ("roles add empty", {"deltas": []}),
        ("roles add missing", {}),
        ("roles add not array", {"deltas": "x"}),
        ("roles remove missing", {"deltas": [{"role": "datalens.admin", "subjectId": ref_id}]}),
        ("roles remove unknown user", {"deltas": [{"role": "datalens.admin", "subjectId": "zz"}]}),
    ]:
        compare(label, "POST", "/v1/management/users/roles/add"
                if "add" in label else "/v1/management/users/roles/remove",
                body, admin_headers, admin_headers)
    compare("profile after roles add", "GET", f"/v1/management/users/{ref_id}/profile",
            headers_ref=admin_headers, headers_rust=admin_headers)

    # update role on two separate users so each service sees the same pre-state
    compare("roles update viewer->editor (reference)", "POST",
            "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.editor",
                         "subjectId": ref_id}]}, admin_headers, admin_headers)
    eq("roles update via rust service", 200,
       http(RUST, "POST", "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.editor",
                         "subjectId": rust_id}]}, headers=admin_headers).status)
    compare("roles update back", "POST", "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "datalens.editor", "newRole": "datalens.viewer",
                         "subjectId": ref_id}]}, admin_headers, admin_headers)
    compare("roles update missing old role", "POST", "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "datalens.admin", "newRole": "datalens.editor",
                         "subjectId": ref_id}]}, admin_headers, admin_headers)
    compare("roles update to existing role", "POST", "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.viewer",
                         "subjectId": ref_id}]}, admin_headers, admin_headers)
    compare("roles update invalid enum", "POST", "/v1/management/users/roles/update",
            {"deltas": [{"oldRole": "bogus", "newRole": "datalens.editor",
                         "subjectId": ref_id}]}, admin_headers, admin_headers)
    compare("roles update not array", "POST", "/v1/management/users/roles/update",
            {"deltas": {}}, admin_headers, admin_headers)
    for role in ["datalens.editor", "datalens.editor"]:
        compare(f"roles remove {role}", "POST", "/v1/management/users/roles/remove",
                {"deltas": [{"role": role, "subjectId": ref_id}]}, admin_headers, admin_headers)
    compare("profile after roles remove", "GET", f"/v1/management/users/{ref_id}/profile",
            headers_ref=admin_headers, headers_rust=admin_headers)

    # --- create validation / duplicates / delete ------------------------------------------
    duplicate_ref = http(REF, "POST", "/v1/management/users/create",
                         {"login": ref_login, "password": strong}, headers=admin_headers)
    duplicate_rust = http(RUST, "POST", "/v1/management/users/create",
                          {"login": ref_login, "password": strong}, headers=admin_headers)
    eq("management duplicate status", duplicate_ref.status, duplicate_rust.status)
    eq("management duplicate body", duplicate_ref.text(), duplicate_rust.text())
    for label, body in [
        ("create invalid login", {"login": "x", "password": strong}),
        ("create weak password", {"login": "authrstest-weak" + suffix, "password": "y"}),
        ("create invalid roles", {"login": "authrstest-roles" + suffix, "password": strong,
                                  "roles": ["bogus"]}),
        ("create empty roles", {"login": "authrstest-emptyroles" + suffix, "password": strong,
                                "roles": []}),
        ("create missing", {}),
        ("create array body", []),
        ("create invalid email", {"login": "authrstest-email" + suffix, "password": strong,
                                  "email": "bad"}),
    ]:
        compare(f"{label} (create)", "POST", "/v1/management/users/create", body,
                admin_headers, admin_headers)

    explicit_login = new_login("explicit")
    explicit_create = http(RUST, "POST", "/v1/management/users/create",
                           {"login": explicit_login, "password": strong,
                            "roles": ["datalens.editor", "datalens.viewer"]},
                           headers=admin_headers)
    eq("create with explicit roles", 200, explicit_create.status)
    explicit_profile = http(REF, "GET",
                            f"/v1/management/users/{explicit_create.json()['userId']}/profile",
                            headers=admin_headers)
    eq("explicit roles profile", ["datalens.editor", "datalens.viewer"],
       explicit_profile.json()["profile"]["roles"])

    delete_a = new_login("deletea")
    delete_b = new_login("deleteb")
    delete_a_create = http(RUST, "POST", "/v1/management/users/create",
                           {"login": delete_a, "password": strong}, headers=admin_headers)
    delete_b_create = http(RUST, "POST", "/v1/management/users/create",
                           {"login": delete_b, "password": strong}, headers=admin_headers)
    delete_a_id = delete_a_create.json()["userId"]
    delete_b_id = delete_b_create.json()["userId"]
    eq("delete via reference", 200,
       http(REF, "DELETE", f"/v1/management/users/{delete_a_id}", headers=admin_headers).status)
    eq("delete via rust service", 200,
       http(RUST, "DELETE", f"/v1/management/users/{delete_b_id}", headers=admin_headers).status)
    for service, base in [("reference", REF), ("rust", RUST)]:
        gone = http(base, "GET", f"/v1/management/users/{delete_a_id}/profile",
                    headers=admin_headers)
        eq(f"deleted user profile ({service})", 404, gone.status)
    compare("delete missing user", "DELETE", "/v1/management/users/123",
            headers_ref=admin_headers, headers_rust=admin_headers)

    # --- IdP-managed users -----------------------------------------------------------------
    idp_a_login = new_login("idpa")
    idp_b_login = new_login("idpb")
    for idp_login in (idp_a_login, idp_b_login):
        psql(
            f"INSERT INTO auth_users (login, password, idp_user_id, idp_slug, idp_type) "
            f"VALUES ('{idp_login}', (SELECT password FROM auth_users WHERE login = 'admin'), "
            f"'ext-{idp_login}', 'saml', 'saml')"
        )
    idp_a_id = encode_id(int(psql(f"SELECT user_id FROM auth_users WHERE login = '{idp_a_login}'")))
    idp_b_id = encode_id(int(psql(f"SELECT user_id FROM auth_users WHERE login = '{idp_b_login}'")))
    compare("IdP profile read", "GET", f"/v1/management/users/{idp_a_id}/profile",
            headers_ref=admin_headers, headers_rust=admin_headers)
    for label, method, path, body in [
        ("IdP public delete", "DELETE", f"/v1/management/users/{idp_a_id}", None),
        ("IdP public profile update", "POST", f"/v1/management/users/{idp_a_id}/profile",
         {"firstName": "Nope"}),
        ("IdP public password update", "POST", f"/v1/management/users/{idp_a_id}/password",
         {"newPassword": strong}),
    ]:
        compare(label, method, path, body, admin_headers, admin_headers)
    compare("IdP private profile update", "POST",
            f"/private/v1/management/users/{idp_a_id}/profile", {"firstName": "Private"},
            master_headers, master_headers)
    compare("IdP private password update", "POST",
            f"/private/v1/management/users/{idp_a_id}/password", {"newPassword": strong},
            master_headers, master_headers)
    eq("IdP private delete via rust service", 200,
       http(RUST, "DELETE", f"/private/v1/management/users/{idp_b_id}",
            headers=master_headers).status)
    compare("IdP deleted profile", "GET", f"/v1/management/users/{idp_b_id}/profile",
            headers_ref=admin_headers, headers_rust=admin_headers)

    # --- private master-token variants -----------------------------------------------------
    for label, headers in [
        ("private without token", {}),
        ("private wrong token", {"x-dl-auth-master-token": "wrong"}),
        ("private substring token", {"x-dl-auth-master-token": "auth"}),
    ]:
        compare(label, "POST", "/private/v1/management/users/create",
                {"login": "authrstest-private" + suffix, "password": strong},
                headers, headers)
    compare("private route with Bearer only", "POST", "/private/v1/management/users/create",
            {"login": "authrstest-private" + suffix, "password": strong},
            admin_headers, admin_headers)
    compare("public route with master token only", "GET", "/v1/users/list",
            headers_ref=master_headers, headers_rust=master_headers)

    private_ref_login = new_login("privref")
    private_rust_login = new_login("privrust")
    compare("private create invalid body", "POST", "/private/v1/management/users/create",
            {"login": "x", "password": "y"}, master_headers, master_headers)
    private_ref = http(REF, "POST", "/private/v1/management/users/create",
                       {"login": private_ref_login, "password": strong}, headers=master_headers)
    private_rust = http(RUST, "POST", "/private/v1/management/users/create",
                        {"login": private_rust_login, "password": strong}, headers=master_headers)
    eq("private create statuses", (200, 200), (private_ref.status, private_rust.status))
    private_ref_id = private_ref.json()["userId"]
    private_rust_id = private_rust.json()["userId"]
    compare("private duplicate create", "POST", "/private/v1/management/users/create",
            {"login": private_ref_login, "password": strong}, master_headers, master_headers)
    compare("private profile update", "POST",
            f"/private/v1/management/users/{private_ref_id}/profile", {"firstName": "Priv"},
            master_headers, master_headers)
    compare("private password update", "POST",
            f"/private/v1/management/users/{private_ref_id}/password",
            {"newPassword": "Zyxwvu9!"}, master_headers, master_headers)
    eq("private password sign-in", 200,
       http(REF, "POST", "/signin",
            {"login": private_ref_login, "password": "Zyxwvu9!"}).status)
    for label, body in [
        ("private roles add", {"deltas": [{"role": "datalens.editor",
                                           "subjectId": private_ref_id}]}),
        ("private roles add invalid", {"deltas": [{"role": "bogus",
                                                   "subjectId": private_ref_id}]}),
        ("private roles remove", {"deltas": [{"role": "datalens.editor",
                                              "subjectId": private_ref_id}]}),
    ]:
        path = "/private/v1/management/users/roles/add" if "add" in label \
            else "/private/v1/management/users/roles/remove"
        compare(label, "POST", path, body, master_headers, master_headers)
    # Rename the role of each service's own scratch user so both see the same pre-state.
    update_ref = http(REF, "POST", "/private/v1/management/users/roles/update",
                      {"deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.editor",
                                   "subjectId": private_ref_id}]}, headers=master_headers)
    update_rust = http(RUST, "POST", "/private/v1/management/users/roles/update",
                       {"deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.editor",
                                    "subjectId": private_rust_id}]}, headers=master_headers)
    eq("private roles update", (200, '{"done":true}'),
       (update_rust.status, update_rust.text()))
    eq("private roles update (reference status)", 200, update_ref.status)
    back_ref = http(RUST, "POST", "/private/v1/management/users/roles/update",
                    {"deltas": [{"oldRole": "datalens.editor", "newRole": "datalens.viewer",
                                 "subjectId": private_ref_id}]}, headers=master_headers)
    back_rust = http(REF, "POST", "/private/v1/management/users/roles/update",
                     {"deltas": [{"oldRole": "datalens.editor", "newRole": "datalens.viewer",
                                  "subjectId": private_rust_id}]}, headers=master_headers)
    eq("private roles update back", (200, '{"done":true}'),
       (back_rust.status, back_rust.text()))
    eq("private roles update back (rust status)", 200, back_ref.status)
    compare("private roles final profile", "GET",
            f"/v1/management/users/{private_rust_id}/profile",
            headers_ref=admin_headers, headers_rust=admin_headers)
    private_delete_a = new_login("privdel")
    private_delete_b = new_login("privdel")
    del_a = http(REF, "POST", "/private/v1/management/users/create",
                 {"login": private_delete_a, "password": strong}, headers=master_headers)
    del_b = http(RUST, "POST", "/private/v1/management/users/create",
                 {"login": private_delete_b, "password": strong}, headers=master_headers)
    eq("private delete reference", 200,
       http(REF, "DELETE", f"/private/v1/management/users/{del_a.json()['userId']}",
            headers=master_headers).status)
    eq("private delete rust", 200,
       http(RUST, "DELETE", f"/private/v1/management/users/{del_b.json()['userId']}",
            headers=master_headers).status)

    # --- wrong-method 404s ----------------------------------------------------------------
    for method, path in [
        ("GET", "/v1/management/users/create"),
        ("POST", "/v1/users/list"),
        ("GET", "/private/v1/management/users/create"),
        ("PUT", "/v1/management/users/roles/add"),
        ("GET", "/v1/users/get-by-ids"),
        ("GET", "/v1/users/me/password"),
    ]:
        compare(f"{method} {path} method mismatch", method, path,
                headers_ref=admin_headers, headers_rust=admin_headers)


# ------------------------------------------------------------------------------------------
# main
# ------------------------------------------------------------------------------------------


def main():
    print(f"reference: {REF}\nrust:      {RUST}\ndatabase:  {DSN}\n")
    reference_ping = http(REF, "GET", "/ping")
    if reference_ping.status != 200:
        raise SystemExit(
            f"reference auth is not reachable at {REF}/ping (start datalens-python-auth-1)"
        )
    try:
        psql("SELECT 1")
    except Exception as error:  # noqa: BLE001
        raise SystemExit(f"pg-auth-db is not reachable via docker exec {PG_CONTAINER}: {error}")

    start_rust(
        RUST_PORT,
        {
            "AUTH_SIGNUP_DISABLED": "false",
            "AUTH_MASTER_TOKEN": MASTER_TOKEN,
        },
    )
    start_rust(DISABLED_PORT, {"AUTH_SIGNUP_DISABLED": "true"})
    disabled_base = f"http://127.0.0.1:{DISABLED_PORT}"

    try:
        test_static_routes()
        reference_signin, rust_signin = test_signin_and_profile()
        test_refresh_interop(reference_signin, rust_signin)
        test_logout()
        test_signup_enabled()
        test_signup_disabled(disabled_base)
        test_user_management()
    finally:
        for proc in procs:
            proc.terminate()
        for proc in procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        cleanup()

    passed = sum(1 for _, ok, _ in checks if ok)
    print(f"\n{passed}/{len(checks)} checks passed")
    if failures:
        print("\nFAILED CHECKS:")
        for name in failures:
            print(f"  - {name}")
        return 1
    print("PARITY OK: no differences against the reference service")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as error:  # noqa: BLE001 - standalone test harness
        print(f"\nHARNESS ERROR: {error}")
        try:
            cleanup()
        except Exception:  # noqa: BLE001
            pass
        sys.exit(2)
