#!/usr/bin/env python3
"""Regression test for chart Y-axis indexing and builder diagnostics.

Two bugs are pinned here:

1. Segmented chart with a Y2 section built one Y axis per segment, while the series carried
   the segment's plot index (0/1, 2/3, ...). ChartKit then rejected e.g. index 3 as out of
   range ("Похоже, что вы пытаетесь использовать некорректный индекс для оси Y: \\"3\\"").
   The chart must answer with a dense `yAxis` array and every `series.yAxis` inside it.
2. A chart without a Y measure used to be reported as the generic
   `chart type graph_wizard_node is not supported by the Rust engine`; the concrete
   `graph: y placeholder is empty` must survive in `error.debug.message`.

The fixture is a real "split by region" demo chart shared config with a Y2 section added; it
needs the demo dataset `qvnkqz8s0wdqf` (the standard demo seed).

Usage:
    python3 scripts/chart_axes_smoke.py
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "fixtures" / "charts" / "segmented_y2_shared.json"
TIMEOUT = 60


class Failure(AssertionError):
    pass


def signin(auth_url: str, login: str, password: str) -> str:
    request = urllib.request.Request(
        f"{auth_url}/signin",
        data=json.dumps({"login": login, "password": password}).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        response = urllib.request.urlopen(request, timeout=TIMEOUT)
    except urllib.error.HTTPError as error:
        raise Failure(f"signin failed: HTTP {error.code}") from error
    cookie = next(
        (value for value in (response.headers.get_all("Set-Cookie") or []) if value.startswith("auth=")),
        None,
    )
    if cookie is None:
        raise Failure("signin returned no auth cookie")
    payload = json.loads(urllib.parse.unquote(cookie.split(";", 1)[0][len("auth="):]))
    return payload["accessToken"]


def run(base_url: str, token: str, shared: dict, workbook_id: str, chart_id: str = "axes-smoke"):
    body = {
        "id": chart_id,
        "params": {},
        "responseOptions": {},
        "workbookId": workbook_id,
        "config": {"data": {"shared": shared}, "meta": {"stype": "d3_wizard_node"}},
    }
    request = urllib.request.Request(
        f"{base_url}/api/run",
        data=json.dumps(body, ensure_ascii=False).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
    )
    try:
        response = urllib.request.urlopen(request, timeout=TIMEOUT)
        return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            return error.code, json.loads(raw)
        except json.JSONDecodeError:
            return error.code, {"raw": raw.decode("utf-8", "replace")}


def assert_dense_axes(data: dict, expected_segments: int) -> None:
    axes = data.get("yAxis")
    series = ((data.get("series") or {}).get("data") or []) if isinstance(data.get("series"), dict) else []
    if not isinstance(axes, list) or not axes:
        raise Failure("chart returned no yAxis array")
    if not series:
        raise Failure("chart returned no series")
    # The fixture has one segment *field*; its distinct values produce one series per side.
    # A Y2 section doubles the axes: one left + one right axis per segment value.
    distinct_segments = len(series) // 2
    if len(axes) != distinct_segments * 2:
        raise Failure(f"expected {distinct_segments * 2} axes for {len(series)} series, got {len(axes)}")
    plots = [axis.get("plotIndex") for axis in axes]
    if any(plot is None for plot in plots) or len(set(plots)) != len(plots):
        raise Failure(f"segmented axes must carry unique plotIndex values, got {plots}")
    indexes = [item.get("yAxis") for item in series]
    for index in indexes:
        if not isinstance(index, int) or not 0 <= index < len(axes):
            raise Failure(f"series yAxis index {index!r} is outside the {len(axes)}-element yAxis array")
    print(f"  ok segmented Y2: axes={len(axes)} series={len(series)} indexes={indexes}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", default="http://127.0.0.1:8099")
    parser.add_argument("--auth-url", default="http://127.0.0.1:8097")
    parser.add_argument("--login", default="admin")
    parser.add_argument("--password", default="admin")
    parser.add_argument("--workbook-id", default="z4wtz6tg5194o")
    parser.add_argument("--fixture", default=str(FIXTURE))
    args = parser.parse_args()

    shared = json.loads(Path(args.fixture).read_text())
    token = signin(args.auth_url, args.login, args.password)
    segments = len(shared.get("segments") or [])
    if segments == 0:
        raise Failure("fixture has no segments")

    status, answer = run(args.base_url, token, shared, args.workbook_id)
    if status != 200:
        raise Failure(f"segmented chart: HTTP {status} {str(answer)[:300]}")
    assert_dense_axes(answer.get("data") or {}, segments)

    # The same fixture without Y/Y2 must keep the builder's concrete 501 text.
    empty = json.loads(json.dumps(shared))
    for placeholder in empty["visualization"]["placeholders"]:
        if placeholder.get("id") in ("y", "y2"):
            placeholder["items"] = []
    status, answer = run(args.base_url, token, empty, args.workbook_id, "axes-empty-smoke")
    message = str(((answer.get("error") or {}).get("debug") or {}).get("message") or "")
    if status == 200 or "y placeholder is empty" not in message:
        raise Failure(
            f"empty-Y chart should report the builder message, got HTTP {status}: {str(answer)[:300]}"
        )
    print(f"  ok empty-Y diagnostic: HTTP {status} {message!r}")

    print("PASS chart axes smoke")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Failure as failure:
        print(f"FAIL: {failure}", file=sys.stderr)
        sys.exit(1)
