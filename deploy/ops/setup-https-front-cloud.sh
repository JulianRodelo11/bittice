#!/usr/bin/env bash
# Apply HTTPS-front compose on cloud EC2 + remind to terraform apply for SG.
#
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/setup-https-front-cloud.sh \
#     --domain dash-sac.prod.parking.net.co \
#     --grpc-domain dash-sac-grpc.prod.parking.net.co
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

APP_NAME="${BITTICE_APP_NAME:-dash-sac-dev}"
SSH_KEY="${BITTICE_SSH_KEY:-${HOME}/.bittice/ssh/bittice_id_ed25519}"
SSH_USER="${BITTICE_SSH_USER:-ubuntu}"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
TF_DIR="${BITTICE_TF_DIR:-${REPO_ROOT}/data/terraform}"
AWS_REGION="${AWS_REGION:-us-east-1}"
IP=""
DOMAIN=""
GRPC_DOMAIN=""
APPLY_TF=0

usage() {
  cat <<'EOF'
Usage: setup-https-front-cloud.sh --domain <rest-host> --grpc-domain <grpc-host> [options]

Options:
  --domain <host>       REST hostname (required)
  --grpc-domain <host>  gRPC hostname (required unless in data/.bittice_cloud.json)
  --ip <addr>           EC2 public IP
  --app-name <name>     EC2 Name tag (default: dash-sac-dev)
  --apply-terraform     Run terraform apply (updates security group to 443/80)
  -h, --help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --domain) DOMAIN="${2:?}"; shift 2 ;;
    --grpc-domain) GRPC_DOMAIN="${2:?}"; shift 2 ;;
    --ip) IP="${2:?}"; shift 2 ;;
    --app-name) APP_NAME="${2:?}"; shift 2 ;;
    --apply-terraform) APPLY_TF=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown: $1" >&2; usage; exit 1 ;;
  esac
done

[[ -n "${DOMAIN}" ]] || { usage; exit 1; }

if [[ -z "${GRPC_DOMAIN}" && -f "${REPO_ROOT}/data/.bittice_cloud.json" ]]; then
  GRPC_DOMAIN="$(python3 - "${REPO_ROOT}/data/.bittice_cloud.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("grpc_domain") or "")
PY
)"
fi

[[ -n "${GRPC_DOMAIN}" ]] || { echo "setup-https-front-cloud: --grpc-domain is required" >&2; usage; exit 1; }

resolve_ip() {
  if [[ -n "${IP}" ]]; then echo "${IP}"; return; fi
  aws ec2 describe-instances --region "${AWS_REGION}" \
    --filters "Name=tag:Name,Values=${APP_NAME}" "Name=instance-state-name,Values=running" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text 2>/dev/null | tr -d '\r'
}

TARGET_IP="$(resolve_ip)"
[[ -n "${TARGET_IP}" && "${TARGET_IP}" != "None" ]] || { echo "Could not resolve EC2 IP" >&2; exit 1; }
[[ -f "${SSH_KEY}" ]] || { echo "SSH key not found: ${SSH_KEY}" >&2; exit 1; }

if [[ "${APPLY_TF}" -eq 1 ]]; then
  TF_BIN="${REPO_ROOT}/data/.terraform-bin/terraform"
  [[ -x "${TF_BIN}" ]] || TF_BIN="$(command -v terraform || true)"
  if [[ -x "${TF_BIN}" && -f "${TF_DIR}/terraform.tfstate" ]]; then
    echo "[setup-https-front-cloud] terraform apply (SG: 443/80/50051 public, 8080 VPC)…" >&2
    "${TF_BIN}" -chdir="${TF_DIR}" apply -auto-approve
  else
    echo "[setup-https-front-cloud] WARN: terraform not found or no state — update SG manually" >&2
  fi
fi

REMOTE_OPS="${DATA_ROOT}/ops"
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" "${SSH_USER}@${TARGET_IP}" "mkdir -p '${REMOTE_OPS}'"
scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" \
  "${SCRIPT_DIR}/setup-https-front.sh" "${SSH_USER}@${TARGET_IP}:${REMOTE_OPS}/"
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "${SSH_KEY}" "${SSH_USER}@${TARGET_IP}" \
  "chmod +x '${REMOTE_OPS}/setup-https-front.sh' && sudo '${REMOTE_OPS}/setup-https-front.sh' --domain '${DOMAIN}' --grpc-domain '${GRPC_DOMAIN}'"

echo "[setup-https-front-cloud] REST → https://${DOMAIN}/" >&2
echo "[setup-https-front-cloud] gRPC → ${GRPC_DOMAIN}:50051" >&2
echo "[setup-https-front-cloud] Admin → ssh -L 8080:127.0.0.1:8080 -i ${SSH_KEY} ${SSH_USER}@${TARGET_IP}" >&2
