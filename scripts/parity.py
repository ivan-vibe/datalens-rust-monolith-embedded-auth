#!/usr/bin/env python3
"""Differential chart-payload parity between this monolith and a reference DataLens stack.

Usage:
    python3 scripts/parity.py --har capture.har \
        --native-run 127.0.0.1:8099 --native-auth 127.0.0.1:8097 \
        --python-run 127.0.0.1:28180 --python-auth 127.0.0.1:28180

`capture.har` is exported from the reference UI (DevTools → Network → Save all as HAR)
after walking through the demo dashboard. Every HAR entry whose URL ends with `/api/run`
and that carries a request body is replayed against both stacks; the parsed JSON answers
are then compared path by path. Exit code 0 = all entries match.

Deliberately allowed to differ: wall-clock fields (`timings`, `latency`, `size`), each
stack's own `dataset_revision_id`, and the generated SQL text (`info.query`).
"""

from __future__ import annotations

import argparse
import base64
import http.client
import json
import sys
import urllib.parse

SKIPPED_SUFFIXES = (
    '/timings',
    '/latency',
    '/size',
    '/data/dataset_revision_id',
    '/info/query',
)


def endpoint(value: str) -> tuple[str, int]:
    host, _, port = value.partition(':')
    return host, int(port)


def request(host_port, method, path, body=None, headers=None, timeout=120):
    connection = http.client.HTTPConnection(*host_port, timeout=timeout)
    merged = {'Content-Type': 'application/json'} if body is not None else {}
    merged.update(headers or {})
    connection.request(method, path, body=body, headers=merged)
    response = connection.getresponse()
    return response.status, response.read().decode('utf-8', 'replace'), response.getheaders()


def signin(host_port):
    status, body, headers = request(
        host_port, 'POST', '/signin', json.dumps({'login': 'admin', 'password': 'admin'})
    )
    if status != 200:
        raise SystemExit(f'signin {host_port[0]}:{host_port[1]} failed: {status} {body[:200]}')
    cookies = [value for name, value in headers if name.lower() == 'set-cookie']
    if not cookies:
        raise SystemExit(f'signin {host_port}: no Set-Cookie in the answer')
    cookie_header = '; '.join(cookie.split(';', 1)[0] for cookie in cookies)
    auth = json.loads(urllib.parse.unquote(cookies[0].split(';', 1)[0].split('=', 1)[1]))
    return {'Authorization': 'Bearer ' + auth['accessToken'], 'Cookie': cookie_header}


def is_run_entry(entry: dict) -> bool:
    url = (entry.get('request') or {}).get('url', '')
    body = (entry.get('request') or {}).get('postData', {}).get('text')
    return url.rstrip('/').endswith('/api/run') and bool(body)


def har_request_body(entry: dict, level0: bool) -> str:
    body = json.loads(entry['request']['postData']['text'])
    if level0:
        body.get('params', {}).pop('drillDownLevel', None)
        body.get('params', {}).pop('drillDownFilters', None)
    return json.dumps(body)


def diff(native, python, path='', out=None):
    out = [] if out is None else out
    if path.endswith(SKIPPED_SUFFIXES):
        return out
    if type(native) is not type(python):
        out.append((path, 'type', type(native).__name__, type(python).__name__))
        return out
    if isinstance(native, dict):
        only_native = sorted(set(native) - set(python))
        only_python = sorted(set(python) - set(native))
        if only_native or only_python:
            out.append((path, 'keys', '+' + str(only_native), '-' + str(only_python)))
        for key in set(native) & set(python):
            diff(native[key], python[key], f'{path}/{key}', out)
    elif isinstance(native, list):
        if len(native) != len(python):
            out.append((path, 'len', len(native), len(python)))
        for index, (left, right) in enumerate(zip(native, python)):
            diff(left, right, f'{path}[{index}]', out)
    elif native != python:
        out.append((path, 'value', repr(native)[:90], repr(python)[:90]))
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--har', required=True, help='HAR export from the reference UI')
    parser.add_argument('--native-run', default='127.0.0.1:8099')
    parser.add_argument('--native-auth', default='127.0.0.1:8097')
    parser.add_argument('--python-run', default='127.0.0.1:28180')
    parser.add_argument('--python-auth', default='127.0.0.1:28180')
    parser.add_argument('--entries', nargs='*', type=int,
                        help='HAR indexes to replay (default: every /api/run entry)')
    parser.add_argument('--level0', action='store_true',
                        help='strip drill-down state so the chart renders its root level')
    args = parser.parse_args()

    entries = json.load(open(args.har))['log']['entries']
    indexes = args.entries or [i for i, entry in enumerate(entries) if is_run_entry(entry)]
    if not indexes:
        raise SystemExit('no /api/run entries found in the HAR')

    native_headers = signin(endpoint(args.native_auth))
    python_headers = signin(endpoint(args.python_auth))

    failures = 0
    for index in indexes:
        entry = entries[index]
        body = har_request_body(entry, args.level0)
        label = f'entry {index} /api/run {entry.get("startedDateTime", "")[11:23]}'
        status, text, _ = request(endpoint(args.native_run), 'POST', '/api/run', body, native_headers)
        native = json.loads(text) if text else None
        status_python, text, _ = request(endpoint(args.python_run), 'POST', '/api/run', body, python_headers)
        python = json.loads(text) if text else None
        if status != status_python:
            print(f'FAIL {label}: HTTP {status} vs {status_python}')
            failures += 1
            continue
        differences = diff(native, python)
        if differences:
            failures += 1
            print(f'FAIL {label}: {len(differences)} diff(s), HTTP {status}')
            for difference in differences[:40]:
                print('   ', difference)
        else:
            print(f'OK   {label}: identical (HTTP {status})')
    print(f'\n{len(indexes) - failures}/{len(indexes)} entries match')
    return 0 if failures == 0 else 1


if __name__ == '__main__':
    sys.exit(main())
