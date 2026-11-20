#!/usr/bin/env bash
# Build the single-binary fork for the host platform (auth is linked in).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release
echo "monolith (with embedded auth): $ROOT/target/release/datalens-monolith"
