#!/usr/bin/env python3
"""Protocol-level regression test for the dataset editor's source/join/refresh flow.

It replays a real editor request captured from the UI (the second-table drop: `add_source`,
`add_source_avatar`, `add_avatar_relation`) against a running monolith and pins the invariants
that broke before the fix:

  * both sources survive the merge (the old code dropped the one that already had fields);
  * every `source_avatars[].source_id` names an existing source;
  * every direct field's `avatar_id` names an existing avatar;
  * field titles are unique (orders and managers both expose `category_name_level_1`);
  * `add/update_avatar_relation` reach the response and an empty relation is reported as the
    normal `ERR.DS_API.REQUEST` until conditions are set;
  * a configured join makes the preview load;
  * `refresh_source` re-reads the schema and keeps the content consistent;
  * a saved dataset round-trips through create + versioned validate + versioned preview.

Prerequisites: the demo stack from deploy/ (`postgres`, demo seed) with the demo `pg` connection
and the `pg-demo-db.orders` / `managers` tables, and a running monolith.

Usage:
    python3 scripts/dataset_editor_smoke.py
    python3 scripts/dataset_editor_smoke.py --base-url http://127.0.0.1:8099 \\
        --auth-url http://127.0.0.1:8097 --login admin --password admin
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "fixtures" / "dataset_editor" / "drop2-request.json"
TIMEOUT = 30


class Failure(AssertionError):
    pass


def request(url: str, method: str = "GET", body=None, token: str | None = None, timeout: int = TIMEOUT):
    headers = {"Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body, ensure_ascii=False).encode()
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, method=method, data=data, headers=headers)
    try:
        response = urllib.request.urlopen(req, timeout=timeout)
        raw = response.read()
        return response.status, json.loads(raw) if raw else {}
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw)
        except json.JSONDecodeError:
            return error.code, {"raw": raw.decode("utf-8", "replace")}


def signin(auth_url: str, login: str, password: str) -> str:
    # The access token is in the `auth` cookie: a URL-encoded JSON envelope.
    req = urllib.request.Request(
        f"{auth_url}/signin",
        data=json.dumps({"login": login, "password": password}).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        response = urllib.request.urlopen(req, timeout=TIMEOUT)
    except urllib.error.HTTPError as error:
        raise Failure(f"signin failed: HTTP {error.code}") from error
    cookies = response.headers.get_all("Set-Cookie") or []
    auth = next((cookie for cookie in cookies if cookie.startswith("auth=")), None)
    if auth is None:
        raise Failure("signin returned no auth cookie")
    value = urllib.parse.unquote(auth.split(";", 1)[0][len("auth="):])
    return json.loads(value)["accessToken"]


def arrays(data: dict, key: str) -> list[dict]:
    value = data.get(key)
    return value if isinstance(value, list) else []


def assert_invariants(dataset: dict, step: str) -> None:
    sources = arrays(dataset, "sources")
    avatars = arrays(dataset, "source_avatars")
    fields = arrays(dataset, "result_schema")
    relations = arrays(dataset, "avatar_relations")
    source_ids = {source.get("id") for source in sources if source.get("id")}
    avatar_ids = {avatar.get("id") for avatar in avatars if avatar.get("id")}

    if not sources:
        raise Failure(f"{step}: no sources in the answer")
    for avatar in avatars:
        if avatar.get("source_id") not in source_ids:
            raise Failure(
                f"{step}: avatar {avatar.get('id')} points at missing source "
                f"{avatar.get('source_id')}"
            )
    for field in fields:
        if field.get("calc_mode") != "direct":
            continue
        if field.get("avatar_id") not in avatar_ids:
            raise Failure(
                f"{step}: field {field.get('guid')!r} points at missing avatar "
                f"{field.get('avatar_id')!r} (UnknownSource)"
            )
    titles = [field.get("title") for field in fields]
    duplicates = sorted({title for title in titles if titles.count(title) > 1})
    if duplicates:
        raise Failure(f"{step}: duplicate field titles {duplicates}")
    for relation in relations:
        for endpoint in ("left_avatar_id", "right_avatar_id"):
            if relation.get(endpoint) not in avatar_ids:
                raise Failure(f"{step}: relation {relation.get('id')} has dangling {endpoint}")
    print(
        f"  ok {step}: sources={len(sources)} avatars={len(avatars)} "
        f"relations={len(relations)} fields={len(fields)}"
    )


def validate(base: str, dataset: dict, updates: list[dict], token: str):
    # The root validator keeps its state in the body; `X-DL-WorkbookId` is not required there.
    return request(
        f"{base}/api/v1/datasets/validators/dataset",
        "POST",
        {"dataset": dataset, "updates": updates},
        token,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8099")
    parser.add_argument("--auth-url", default="http://127.0.0.1:8097")
    parser.add_argument("--login", default="admin")
    parser.add_argument("--password", default="admin")
    parser.add_argument("--workbook-id", default=None)
    parser.add_argument("--fixture", default=str(FIXTURE))
    parser.add_argument("--keep", action="store_true", help="keep the dataset created by the test")
    args = parser.parse_args()

    fixture = json.loads(Path(args.fixture).read_text())
    workbook_id = args.workbook_id or fixture.get("workbookId")
    payload = fixture.get("data", fixture)
    token = signin(args.auth_url, args.login, args.password)

    # 1. The captured second-table drop: previous content (orders) + add managers/relation.
    status, answer = validate(args.base_url, payload["dataset"], payload["updates"], token)
    if status != 200 or answer.get("code") != "OK":
        raise Failure(f"second drop validation: HTTP {status} {answer.get('code')} {answer.get('message')}")
    dataset = answer["dataset"]
    assert_invariants(dataset, "second drop")
    if len(arrays(dataset, "avatar_relations")) != 1:
        raise Failure("second drop: the editor's add_avatar_relation was lost")

    # 2. Until the join is configured the preview must fail with the documented 2-source error.
    status, preview = request(
        f"{args.base_url}/api/data/v1/datasets/data/preview",
        "POST",
        {"dataset": dataset, "limit": 10},
        token,
    )
    if status != 400 or "avatar relation" not in json.dumps(preview):
        raise Failure(f"preview before join should be the normal 2-source error, got HTTP {status}: {preview}")
    print("  ok preview before join: HTTP 400 'no avatar relation joins them together' (expected)")

    # 3. Configure the join on the first column both tables share.
    def columns(source: dict) -> list[str]:
        return [
            column["name"]
            for column in arrays(source, "raw_schema")
            if isinstance(column, dict) and column.get("name")
        ]

    left, right = arrays(dataset, "sources")[:2]
    common = [name for name in columns(left) if name in columns(right)]
    if not common:
        raise Failure("fixture sources have no common column to join on")
    relation = arrays(dataset, "avatar_relations")[0]
    relation["conditions"] = [
        {
            "operator": "eq",
            "left": {"source": common[0], "calc_mode": "direct"},
            "right": {"source": common[0], "calc_mode": "direct"},
            "type": "binary",
        }
    ]
    status, answer = validate(
        args.base_url,
        dataset,
        [{"action": "update_avatar_relation", "avatar_relation": relation}],
        token,
    )
    if status != 200 or answer.get("code") != "OK":
        raise Failure(f"join update: HTTP {status} {answer.get('code')} {answer.get('message')}")
    joined = answer["dataset"]
    assert_invariants(joined, "join update")
    if len(arrays(joined["avatar_relations"][0], "conditions")) != 1:
        raise Failure("join update: conditions did not reach the answer")

    status, preview = request(
        f"{args.base_url}/api/data/v1/datasets/data/preview",
        "POST",
        {"dataset": joined, "limit": 10},
        token,
    )
    rows = preview.get("result", {}).get("data", {}).get("Data", []) if status == 200 else []
    if status != 200 or not rows:
        raise Failure(f"preview after join: HTTP {status} {str(preview)[:200]}")
    print(f"  ok preview after join: HTTP 200, {len(rows)} rows")

    # 4. Save and refresh: refresh_source must re-read the schema and stay consistent.
    name = f"smoke-dataset-{int(time.time())}"
    status, created = request(
        f"{args.base_url}/api/v1/datasets",
        "POST",
        {"dataset": joined, "workbook_id": workbook_id, "name": name},
        token,
    )
    if status != 200 or not created.get("id"):
        raise Failure(f"create: HTTP {status} {str(created)[:200]}")
    dataset_id = created["id"]
    try:
        refresh = [
            {"action": "refresh_source", "source": {"id": source["id"]}}
            for source in arrays(joined, "sources")
        ]
        status, answer = request(
            f"{args.base_url}/api/v1/datasets/{dataset_id}/versions/draft/validators/schema",
            "POST",
            {"dataset": joined, "updates": refresh},
            token,
        )
        if status != 200 or answer.get("code") != "OK":
            raise Failure(f"refresh: HTTP {status} {answer.get('code')} {answer.get('message')}")
        refreshed = answer["dataset"]
        assert_invariants(refreshed, "refresh")
        if len(arrays(refreshed, "sources")) != len(arrays(joined, "sources")):
            raise Failure("refresh: lost a source")
        print("  ok refresh_source: schema re-read, content consistent")

        status, versioned = request(
            f"{args.base_url}/api/data/v1/datasets/{dataset_id}/versions/draft/preview",
            "POST",
            {"dataset": refreshed, "limit": 10},
            token,
        )
        if status != 200:
            raise Failure(f"versioned preview: HTTP {status} {str(versioned)[:200]}")
        print("  ok saved dataset preview: HTTP 200")
    finally:
        if args.keep:
            print(f"  kept dataset {dataset_id} ({name})")
        else:
            status, _ = request(f"{args.base_url}/api/v1/datasets/{dataset_id}", "DELETE", token=token)
            print(f"  cleanup dataset {dataset_id}: HTTP {status}")

    print("PASS dataset editor smoke")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failure as failure:
        print(f"FAIL: {failure}", file=sys.stderr)
        sys.exit(1)
