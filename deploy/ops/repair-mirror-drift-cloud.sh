#!/usr/bin/env bash
# Run repair-mirror-drift.sh on a cloud EC2 via SSH (same pattern as check-mirror-cloud.sh).
#
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/repair-mirror-drift-cloud.sh --entity bittice_host --yes
#   ./deploy/ops/repair-mirror-drift-cloud.sh --entity bittice_host --yes --table db_attendant_dev.pagos
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

APP_NAME="${BITTICE_APP_NAME:-dash-sac-dev}"
SSH_KEY="${BITTICE_SSH_KEY:-${HOME}/.bittice/ssh/bittice_id_ed25519}"
SSH_USER="${BITTICE_SSH_USER:-ubuntu}"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
AWS_REGION="${AWS_REGION:-us-east-1}"
IP=""

EXTRA_ARGS=()

usage() {
  cat <<'EOF'
Usage: repair-mirror-drift-cloud.sh [options] [-- repair-args...]

Options:
  --ip <addr>          EC2 public IP (skip AWS lookup).
  --app-name <name>    EC2 Name tag (default: dash-sac-dev).
  -h, --help           Show help.

All other flags are forwarded to repair-mirror-drift.sh on the instance
(--entity, --table, --yes, --dry-run, --diagnose, --skip-check).

Examples:
  AWS_PROFILE=deploy-goparking ./deploy/ops/repair-mirror-drift-cloud.sh --entity bittice_host --yes
  AWS_PROFILE=deploy-goparking ./deploy/ops/repair-mirror-drift-cloud.sh --entity bittice_host --dry-run
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ip) IP="${2:?}"; shift 2 ;;
    --app-name) APP_NAME="${2:?}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; EXTRA_ARGS+=("$@"); break ;;
    *) EXTRA_ARGS+=("$1"); shift ;;
  esac
done

resolve_ip() {
  if [[ -n "${IP}" ]]; then
    echo "${IP}"
    return 0
  fi
  aws ec2 describe-instances \
    --region "${AWS_REGION}" \
    --filters \
      "Name=tag:Name,Values=${APP_NAME}" \
      "Name=instance-state-name,Values=running" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' \
    --output text 2>/dev/null | tr -d '\r'
}

TARGET_IP="$(resolve_ip)"
if [[ -z "${TARGET_IP}" || "${TARGET_IP}" == "None" ]]; then
  echo "[repair-mirror-drift-cloud] could not resolve EC2 IP (tag Name=${APP_NAME})" >&2
  exit 1
fi
if [[ ! -f "${SSH_KEY}" ]]; then
  echo "[repair-mirror-drift-cloud] SSH key not found: ${SSH_KEY}" >&2
  exit 1
fi

REMOTE_OPS="${DATA_ROOT}/ops"
echo "[repair-mirror-drift-cloud] ssh ${SSH_USER}@${TARGET_IP} (app=${APP_NAME})" >&2

# Sync ops scripts to the instance (idempotent).
ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=25 \
    -i "${SSH_KEY}" \
    "${SSH_USER}@${TARGET_IP}" \
    "mkdir -p '${REMOTE_OPS}'"

scp -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -i "${SSH_KEY}" \
    "${SCRIPT_DIR}/repair-mirror-drift.sh" \
    "${SCRIPT_DIR}/diagnose-mirror-drift.py" \
    "${SSH_USER}@${TARGET_IP}:${REMOTE_OPS}/"

ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -i "${SSH_KEY}" \
    "${SSH_USER}@${TARGET_IP}" \
    "chmod +x '${REMOTE_OPS}/repair-mirror-drift.sh' && BITTICE_DATA_ROOT='${DATA_ROOT}' sudo '${REMOTE_OPS}/repair-mirror-drift.sh' ${EXTRA_ARGS[*]:-}"
