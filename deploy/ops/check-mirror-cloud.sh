#!/usr/bin/env bash
# Compare MySQL row counts vs Bittice mirror on a cloud EC2 (same as `bittice check-mirror`
# locally, but runs on the instance that holds /opt/bittice/data and can reach private RDS).
#
# Run from your laptop:
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/check-mirror-cloud.sh
#   ./deploy/ops/check-mirror-cloud.sh --entity bittice_host --revalidate
#   ./deploy/ops/check-mirror-cloud.sh --ip 100.52.37.147 --table BpCliente
#   ./deploy/ops/check-mirror-cloud.sh --table beparking.BpCliente
#   ./deploy/ops/check-mirror-cloud.sh --table attendant/pagos
#
# Run on the EC2 itself (e.g. after scp to /opt/bittice/ops/):
#   ./check-mirror-cloud.sh --local --entity bittice_host
#
# Exit code: 0 all tables match, 1 drift (or error).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

APP_NAME=""
SSH_KEY="${BITTICE_SSH_KEY:-${HOME}/.bittice/ssh/bittice_id_ed25519}"
SSH_USER="${BITTICE_SSH_USER:-ubuntu}"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
IMAGE="${BITTICE_IMAGE:-}"
TF_DIR="${BITTICE_TF_DIR:-${REPO_ROOT}/data/terraform}"
AWS_REGION="${AWS_REGION:-us-east-1}"

LOCAL_MODE=0
IP=""
ENTITY=""
TABLE=""
REVALIDATE=0
JSON=0
EXTRA_ARGS=()

usage() {
  cat <<'EOF'
Usage: check-mirror-cloud.sh [options] [-- check-mirror-args...]

Options:
  --local              Run on this machine (EC2). Skip AWS/SSH.
  --ip <addr>          EC2 public IP (skip AWS/terraform lookup).
  --app-name <name>    EC2 Name tag (default: from .bittice_cloud.json or terraform.tfvars).
  --entity <profile>   CDC profile under data/profiles/ (e.g. parking_host), NOT the EC2 name.
  --table <filter>     Table filter (repeatable via extra args). Examples:
                         BpCliente              — any bootstrapped schema
                         beparking.BpCliente    — partial schema match
                         db_beparking_prod.BpCliente — full bootstrapped key
  --revalidate         Re-read counts after 2s when drift is detected.
  --json               JSON output.
  -h, --help           Show this help.

Environment:
  AWS_PROFILE          AWS CLI profile (e.g. deploy-goparking).
  BITTICE_SSH_KEY      Private key (default: ~/.bittice/ssh/bittice_id_ed25519).
  BITTICE_IMAGE        Override bittice image (default: running container's image).
  BITTICE_TF_DIR       Terraform dir for `terraform output public_ip` fallback.

Examples:
  AWS_PROFILE=deploy-goparking ./deploy/ops/check-mirror-cloud.sh
  AWS_PROFILE=deploy-goparking ./deploy/ops/check-mirror-cloud.sh --entity bittice_host --revalidate
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --local) LOCAL_MODE=1; shift ;;
    --ip) IP="${2:?}"; shift 2 ;;
    --app-name) APP_NAME="${2:?}"; shift 2 ;;
    --entity) ENTITY="${2:?}"; shift 2 ;;
    --table) TABLE="${2:?}"; shift 2 ;;
    --revalidate) REVALIDATE=1; shift ;;
    --json) JSON=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; EXTRA_ARGS+=("$@"); break ;;
    *) EXTRA_ARGS+=("$1"); shift ;;
  esac
done

# shellcheck source=cloud_common.sh
source "${SCRIPT_DIR}/cloud_common.sh"
if [[ -z "${APP_NAME}" ]]; then
  APP_NAME="$(resolve_default_app_name "${REPO_ROOT}")"
fi

run_check_mirror_on_host() {
  local img="${IMAGE}"
  if [[ -z "${img}" ]] && docker inspect bittice >/dev/null 2>&1; then
    img="$(docker inspect bittice --format '{{.Config.Image}}')"
  fi
  if [[ -z "${img}" ]]; then
    img="ghcr.io/julianrodelo11/bittice:stable"
  fi

  local -a cmd=(check-mirror)
  [[ -n "${ENTITY}" ]] && cmd+=(--entity "${ENTITY}")
  [[ -n "${TABLE}" ]] && cmd+=(--table "${TABLE}")
  [[ "${REVALIDATE}" -eq 1 ]] && cmd+=(--revalidate)
  [[ "${JSON}" -eq 1 ]] && cmd+=(--json)
  cmd+=("${EXTRA_ARGS[@]}")

  echo "[check-mirror-cloud] image=${img} data=${DATA_ROOT} cmd=${cmd[*]}" >&2

  # --network host: reach private RDS the same way the running engine does.
  docker run --rm --network host \
    -v "${DATA_ROOT}:/app/data" \
    -e BITTICE_DATA_ROOT=/app/data \
    "${img}" \
    "${cmd[@]}"
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
  if [[ ! -f "${TF_DIR}/terraform.tfstate" ]]; then
    return 1
  fi
  if ! command -v terraform >/dev/null 2>&1; then
    return 1
  fi
  terraform -chdir="${TF_DIR}" output -raw public_ip 2>/dev/null | tr -d '\r'
}

resolve_ip() {
  local candidate=""
  if [[ -n "${IP}" ]]; then
    echo "${IP}"
    return 0
  fi

  candidate="$(resolve_ip_aws || true)"
  if [[ -n "${candidate}" && "${candidate}" != "None" ]]; then
    echo "${candidate}"
    return 0
  fi

  candidate="$(resolve_ip_terraform || true)"
  if [[ -n "${candidate}" ]]; then
    echo "${candidate}"
    return 0
  fi

  echo "[check-mirror-cloud] could not resolve EC2 IP (tag Name=${APP_NAME}). Use --app-name dash-sac-prod, --ip, or fix AWS_PROFILE." >&2
  return 1
}

if [[ "${LOCAL_MODE}" -eq 1 ]]; then
  run_check_mirror_on_host
  exit $?
fi

TARGET_IP="$(resolve_ip)"
if [[ ! -f "${SSH_KEY}" ]]; then
  echo "[check-mirror-cloud] SSH key not found: ${SSH_KEY}" >&2
  exit 1
fi

REMOTE_ENV=(
  "BITTICE_DATA_ROOT=${DATA_ROOT}"
)
[[ -n "${IMAGE}" ]] && REMOTE_ENV+=("BITTICE_IMAGE=${IMAGE}")
[[ -n "${ENTITY}" ]] && REMOTE_ENV+=("CM_ENTITY=${ENTITY}")
[[ -n "${TABLE}" ]] && REMOTE_ENV+=("CM_TABLE=${TABLE}")
[[ "${REVALIDATE}" -eq 1 ]] && REMOTE_ENV+=("CM_REVALIDATE=1")
[[ "${JSON}" -eq 1 ]] && REMOTE_ENV+=("CM_JSON=1")

echo "[check-mirror-cloud] ssh ${SSH_USER}@${TARGET_IP} (app=${APP_NAME})" >&2

ssh -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ConnectTimeout=25 \
    -i "${SSH_KEY}" \
    "${SSH_USER}@${TARGET_IP}" \
    env "${REMOTE_ENV[@]}" CM_EXTRA="${EXTRA_ARGS[*]:-}" bash -s <<'REMOTE'
set -euo pipefail
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
IMAGE="${BITTICE_IMAGE:-}"
if [[ -z "${IMAGE}" ]] && docker inspect bittice >/dev/null 2>&1; then
  IMAGE="$(docker inspect bittice --format '{{.Config.Image}}')"
fi
[[ -z "${IMAGE}" ]] && IMAGE="ghcr.io/julianrodelo11/bittice:stable"

cmd=(check-mirror)
[[ -n "${CM_ENTITY:-}" ]] && cmd+=(--entity "${CM_ENTITY}")
[[ -n "${CM_TABLE:-}" ]] && cmd+=(--table "${CM_TABLE}")
[[ "${CM_REVALIDATE:-0}" == "1" ]] && cmd+=(--revalidate)
[[ "${CM_JSON:-0}" == "1" ]] && cmd+=(--json)
if [[ -n "${CM_EXTRA:-}" ]]; then
  # shellcheck disable=SC2206
  extra=(${CM_EXTRA})
  cmd+=("${extra[@]}")
fi

docker run --rm --network host \
  -v "${DATA_ROOT}:/app/data" \
  -e BITTICE_DATA_ROOT=/app/data \
  "${IMAGE}" \
  "${cmd[@]}"
REMOTE
