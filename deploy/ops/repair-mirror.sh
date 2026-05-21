#!/usr/bin/env bash
# Fleet ops: compact over-segmented mirror tables (heartbeat UPDATE micro-segments).
# Requires a bittice image that includes the `compact-mirror` CLI (v0.1.136+).
#
# Usage on EC2 (engine stopped briefly):
#   sudo /opt/bittice/ops/repair-mirror.sh bittice deployments usage_hours api_keys
#
# Run after Watchtower has pulled the new :stable image (tag → Actions → GHCR → EC2).
# Override only for debugging: BITTICE_IMAGE=ghcr.io/.../bittice:v0.1.136
set -euo pipefail

ENTITY="${1:?entity (e.g. bittice)}"
shift
TABLES=("$@")
IMAGE="${BITTICE_IMAGE:-ghcr.io/julianrodelo11/bittice:stable}"
DATA="${BITTICE_DATA_ROOT:-/opt/bittice/data}"

log() { echo "[repair-mirror] $*"; }

if [ "${#TABLES[@]}" -eq 0 ]; then
  log "Compacting all tables under mirror/${ENTITY}…"
  docker run --rm \
    -v "${DATA}:/app/data" \
    -e BITTICE_DATA_ROOT=/app/data \
    "${IMAGE}" \
    compact-mirror "${ENTITY}" --all-tables
  exit 0
fi

log "Stopping bittice container for safe offline compact…"
docker stop bittice

for t in "${TABLES[@]}"; do
  log "Compact ${ENTITY}/${t}"
  docker run --rm \
    -v "${DATA}:/app/data" \
    -e BITTICE_DATA_ROOT=/app/data \
    "${IMAGE}" \
    compact-mirror "${ENTITY}" "${t}"
done

log "Starting bittice…"
docker start bittice
docker ps --filter name=bittice
