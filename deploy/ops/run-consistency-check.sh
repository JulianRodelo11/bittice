#!/usr/bin/env bash
# Fleet ops: run consistency_check_reporter.py on the EC2 host (not inside the motor).
set -euo pipefail

OPS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
ENV_FILE="${OPS_DIR}/runtime.env"
FLUSH_ENV="${OPS_DIR}/flush-lambda.env"
CONTAINER="${BITTICE_CONTAINER_NAME:-bittice}"
# /var/log often needs root on first write; fall back to ops dir
LOG_FILE="${CONSISTENCY_LOG:-/var/log/bittice-consistency.log}"
if [[ ! -w "$(dirname "${LOG_FILE}")" ]] 2>/dev/null; then
  LOG_FILE="${OPS_DIR}/consistency.log"
fi

log() { echo "[consistency-check] $(date -Is) $*"; }

if ! command -v python3 >/dev/null 2>&1; then
  log "python3 not found. Run: sudo apt-get install -y python3 python3-pip"
  exit 1
fi

if ! python3 -c "import pymysql" 2>/dev/null; then
  if command -v apt-get >/dev/null 2>&1; then
    log "Installing python3-pymysql (apt)..."
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3-pymysql
  elif python3 -m pip --version >/dev/null 2>&1; then
    log "Installing pymysql (pip)..."
    python3 -m pip install --user -q -r "${OPS_DIR}/requirements.txt"
  else
    log "pymysql missing. Install: sudo apt-get install -y python3-pymysql"
    exit 1
  fi
fi

if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "${CONTAINER}"; then
  log "Loading control-plane env from container ${CONTAINER}..."
  docker inspect "${CONTAINER}" --format '{{range .Config.Env}}{{println .}}{{end}}' \
    | grep -E '^BITTICE_(DEPLOYMENT_ID|INSTANCE_TOKEN|CONTROL_PLANE_URL)=' \
    > "${ENV_FILE}" || true
  chmod 600 "${ENV_FILE}" 2>/dev/null || true
else
  log "Container ${CONTAINER} not running — using existing ${ENV_FILE} if present."
fi

export BITTICE_DATA_ROOT="${DATA_ROOT}"
if [[ -f "${ENV_FILE}" ]]; then
  set -a
  # shellcheck source=/dev/null
  source "${ENV_FILE}"
  set +a
fi
if [[ -f "${FLUSH_ENV}" ]]; then
  set -a
  # shellcheck source=/dev/null
  source "${FLUSH_ENV}"
  set +a
fi

log "start"
exec python3 "${OPS_DIR}/consistency_check_reporter.py" "$@"
