#!/usr/bin/env bash
# Build the image from a pre-built Linux binary (same flow as CI).
# Example (musl linux x86_64, as in release):
#   cargo build --release --target x86_64-unknown-linux-musl
#   ./deploy/scripts/build-image-from-binary.sh target/x86_64-unknown-linux-musl/release/bittice
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

BINARY="${1:?path to Linux bittice binary (ELF)}"
TAG="${2:-bittice:local}"
STAGING="bittice-linux-staged"
cp -f "$BINARY" "$STAGING"

cleanup() { rm -f "$ROOT/$STAGING"; }
trap cleanup EXIT

docker build -f deploy/Dockerfile --build-arg BINARY_PATH="$STAGING" -t "$TAG" .
