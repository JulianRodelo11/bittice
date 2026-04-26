#!/usr/bin/env bash
# Build the image by compiling Bittice inside Docker (no Rust toolchain on the host).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

TAG="${1:-bittice:local}"

exec docker build -f deploy/Dockerfile.from-source -t "$TAG" .
