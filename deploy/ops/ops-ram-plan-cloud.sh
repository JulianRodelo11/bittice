#!/usr/bin/env bash
# Run ops-ram-plan on cloud EC2 via SSH.
#
#   AWS_PROFILE=deploy-goparking-prod BITTICE_APP_NAME=dash-sac-prod \
#     ./deploy/ops/ops-ram-plan-cloud.sh
#   ./deploy/ops/ops-ram-plan-cloud.sh --apply
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=cloud_common.sh
source "${SCRIPT_DIR}/cloud_common.sh"

REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
APP_NAME="$(resolve_default_app_name "${REPO_ROOT}")"
SSH_KEY="${BITTICE_SSH_KEY:-${HOME}/.bittice/ssh/bittice_id_ed25519}"
SSH_USER="${BITTICE_SSH_USER:-ubuntu}"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
AWS_REGION="${AWS_REGION:-us-east-1}"
IP=""
EXTRA_ARGS=()

usage() {
  cat <<'EOF'
Usage: ops-ram-plan-cloud.sh [options] [-- ops-ram-plan.py args]

Options:
  --ip <addr>       EC2 public IP (skip AWS lookup)
  --app-name <name> EC2 Name tag
  -h, --help

Examples:
  ./deploy/ops/ops-ram-plan-cloud.sh
  ./deploy/ops/ops-ram-plan-cloud.sh --apply
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
  echo "[ops-ram-plan-cloud] could not resolve EC2 IP (tag Name=${APP_NAME})" >&2
  exit 1
fi
if [[ ! -f "${SSH_KEY}" ]]; then
  echo "[ops-ram-plan-cloud] SSH key not found: ${SSH_KEY}" >&2
  exit 1
fi

REMOTE_OPS="${DATA_ROOT}/ops"
echo "[ops-ram-plan-cloud] ssh ${SSH_USER}@${TARGET_IP} (app=${APP_NAME})" >&2

ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25 \
  -i "${SSH_KEY}" "${SSH_USER}@${TARGET_IP}" "mkdir -p '${REMOTE_OPS}'"

scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" \
  "${SCRIPT_DIR}/ops-ram-plan.py" \
  "${SCRIPT_DIR}/ops-ram-plan.sh" \
  "${SSH_USER}@${TARGET_IP}:${REMOTE_OPS}/"

ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" \
  "${SSH_USER}@${TARGET_IP}" \
  "chmod +x '${REMOTE_OPS}/ops-ram-plan.sh' && BITTICE_DATA_ROOT='${DATA_ROOT}' sudo '${REMOTE_OPS}/ops-ram-plan.sh' ${EXTRA_ARGS[*]:-}"
