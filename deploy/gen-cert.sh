#!/usr/bin/env bash
# Generate the self-signed certificate for the HTTP/2 UI listener (host :8092 -> nginx :8443).
# Replace with a trusted mkcert/CA certificate for normal browser use.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CERT="$ROOT/deploy/certs/localhost.crt"
KEY="$ROOT/deploy/certs/localhost.key"
if [ -s "$CERT" ] && [ -s "$KEY" ]; then
  exit 0
fi
mkdir -p "$ROOT/deploy/certs"
openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
  -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -keyout "$KEY" -out "$CERT" >/dev/null 2>&1
chmod 600 "$KEY"
chmod 644 "$CERT"
echo "generated self-signed HTTP/2 certificate: $CERT"
