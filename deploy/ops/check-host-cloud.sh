#!/usr/bin/env bash
# Host resource snapshot on the Bittice EC2 (CPU, memory, disk, Docker).
#
# Run from your laptop:
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/check-host-cloud.sh
#   ./deploy/ops/check-host-cloud.sh --json
#
# Run on the EC2 itself:
#   ./check-host-cloud.sh --local
#
# Exit code: 0 OK, 1 warning, 2 critical/error.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

APP_NAME="${BITTICE_APP_NAME:-dash-sac-dev}"
SSH_KEY="${BITTICE_SSH_KEY:-${HOME}/.bittice/ssh/bittice_id_ed25519}"
SSH_USER="${BITTICE_SSH_USER:-ubuntu}"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
TF_DIR="${BITTICE_TF_DIR:-${REPO_ROOT}/data/terraform}"
AWS_REGION="${AWS_REGION:-us-east-1}"
PY="${SCRIPT_DIR}/check-host-resources.py"

LOCAL_MODE=0
IP=""
JSON=0

usage() {
  cat <<'EOF'
Usage: check-host-cloud.sh [options]

Options:
  --local              Run on this machine (EC2). Skip AWS/SSH.
  --ip <addr>          EC2 public IP (skip AWS/terraform lookup).
  --app-name <name>    EC2 Name tag (default: dash-sac-dev).
  --json               JSON output.
  -h, --help           Show this help.

Environment:
  AWS_PROFILE          AWS CLI profile (e.g. deploy-goparking).
  BITTICE_SSH_KEY      Private key (default: ~/.bittice/ssh/bittice_id_ed25519).
  BITTICE_DATA_ROOT    Default /opt/bittice/data.

Examples:
  AWS_PROFILE=deploy-goparking ./deploy/ops/check-host-cloud.sh
  AWS_PROFILE=deploy-goparking ./deploy/ops/check-host-cloud.sh --json
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --local) LOCAL_MODE=1; shift ;;
    --ip) IP="${2:?}"; shift 2 ;;
    --app-name) APP_NAME="${2:?}"; shift 2 ;;
    --json) JSON=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
  esac
done

run_on_host() {
  local -a args=(--data-root "${DATA_ROOT}")
  [[ "${JSON}" -eq 1 ]] && args+=(--json)
  python3 "${PY}" "${args[@]}"
}

resolve_ip_aws() {
  aws ec2 describe-instances \
    --region "${AWS_REGION}" \
    --filters \
      "Name=tag:Name,Values=${APP_NAME}" \
      "Name=instance-state-name,Values=running" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' \
    --output text 2>/dev/null | tr -d '\r'
}

resolve_ip_terraform() {
  [[ -f "${TF_DIR}/terraform.tfstate" ]] || return 1
  command -v terraform >/dev/null 2>&1 || return 1
  terraform -chdir="${TF_DIR}" output -raw public_ip 2>/dev/null | tr -d '\r'
}

resolve_ip() {
  if [[ -n "${IP}" ]]; then echo "${IP}"; return 0; fi
  local candidate
  candidate="$(resolve_ip_aws || true)"
  if [[ -n "${candidate}" && "${candidate}" != "None" ]]; then echo "${candidate}"; return 0; fi
  candidate="$(resolve_ip_terraform || true)"
  if [[ -n "${candidate}" ]]; then echo "${candidate}"; return 0; fi
  echo "[check-host-cloud] could not resolve EC2 IP (tag Name=${APP_NAME}). Use --ip or fix AWS_PROFILE." >&2
  return 1
}

if [[ "${LOCAL_MODE}" -eq 1 ]]; then
  run_on_host
  exit $?
fi

TARGET_IP="$(resolve_ip)"
if [[ ! -f "${SSH_KEY}" ]]; then
  echo "[check-host-cloud] SSH key not found: ${SSH_KEY}" >&2
  exit 2
fi

echo "[check-host-cloud] ssh ${SSH_USER}@${TARGET_IP} (app=${APP_NAME})" >&2

REMOTE_OPS="${DATA_ROOT}/ops"
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25 \
  -i "${SSH_KEY}" "${SSH_USER}@${TARGET_IP}" "mkdir -p '${REMOTE_OPS}'"
scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" \
  "${PY}" "${SSH_USER}@${TARGET_IP}:${REMOTE_OPS}/check-host-resources.py" >/dev/null

REMOTE_JSON=0
[[ "${JSON}" -eq 1 ]] && REMOTE_JSON=1

ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25 \
  -i "${SSH_KEY}" "${SSH_USER}@${TARGET_IP}" \
  "BITTICE_DATA_ROOT='${DATA_ROOT}' CHECK_HOST_JSON='${REMOTE_JSON}' bash -s" <<'REMOTE'
set -euo pipefail
args=(--data-root "${BITTICE_DATA_ROOT:-/opt/bittice/data}")
[[ "${CHECK_HOST_JSON:-0}" == "1" ]] && args+=(--json)
python3 "${BITTICE_DATA_ROOT:-/opt/bittice/data}/ops/check-host-resources.py" "${args[@]}"
REMOTE
