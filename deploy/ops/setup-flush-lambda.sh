#!/usr/bin/env bash
# One-time: deploy VPC Lambda that flushes MySQL host_cache. EC2 calls it on error 1129.
#
# Prereqs: AWS CLI, python3, pip, bittice-db/.env (DB_*), AWS profile with
# lambda + ec2 + rds IAM permissions.
#
# Usage (from bittice repo root):
#   AWS_PROFILE=bittice deploy/ops/setup-flush-lambda.sh
#
# Writes deploy/ops/flush-lambda.env — copy to EC2 as /opt/bittice/ops/flush-lambda.env
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ENV_FILE="${BITTICE_DB_ENV:-${REPO_ROOT}/../bittice-db/.env}"
OUT_ENV="${SCRIPT_DIR}/flush-lambda.env"

PROFILE="${AWS_PROFILE:-bittice}"
REGION="${AWS_REGION:-us-east-1}"
FUNCTION_NAME="${FLUSH_LAMBDA_NAME:-bittice-flush-host-cache}"
RDS_INSTANCE="${RDS_INSTANCE_ID:-bittice}"
SECRET="$(openssl rand -hex 24 2>/dev/null || python3 -c 'import secrets; print(secrets.token_hex(24))')"

log() { echo "[setup-flush-lambda] $*"; }

if [[ ! -f "${ENV_FILE}" ]]; then
  echo "Missing ${ENV_FILE}" >&2
  exit 1
fi
set -a
# shellcheck source=/dev/null
source "${ENV_FILE}"
set +a

for v in DB_HOST DB_USER DB_PASS; do
  if [[ -z "${!v:-}" ]]; then
    echo "Missing ${v} in ${ENV_FILE}" >&2
    exit 1
  fi
done

log "Discovering RDS ${RDS_INSTANCE} networking…"
read -r VPC_ID RDS_SG SUBNETS <<<"$(
  aws rds describe-db-instances \
    --profile "${PROFILE}" --region "${REGION}" \
    --db-instance-identifier "${RDS_INSTANCE}" \
    --query 'DBInstances[0].[DBSubnetGroup.VpcId,VpcSecurityGroups[0].VpcSecurityGroupId,join(`,`,DBSubnetGroup.Subnets[*].SubnetIdentifier)]' \
    --output text
)"
IFS=',' read -ra SUBNET_ARR <<<"${SUBNETS}"
if [[ -z "${VPC_ID}" || "${VPC_ID}" == "None" ]]; then
  echo "Could not resolve VPC for RDS ${RDS_INSTANCE}" >&2
  exit 1
fi

LAMBDA_SG_NAME="${FUNCTION_NAME}-sg"
EXISTING_SG="$(aws ec2 describe-security-groups \
  --profile "${PROFILE}" --region "${REGION}" \
  --filters "Name=group-name,Values=${LAMBDA_SG_NAME}" "Name=vpc-id,Values=${VPC_ID}" \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)"
if [[ -n "${EXISTING_SG}" && "${EXISTING_SG}" != "None" ]]; then
  LAMBDA_SG="${EXISTING_SG}"
  log "Reusing Lambda SG ${LAMBDA_SG}"
else
  LAMBDA_SG="$(aws ec2 create-security-group \
    --profile "${PROFILE}" --region "${REGION}" \
    --group-name "${LAMBDA_SG_NAME}" \
    --description "Lambda flush host_cache for ${RDS_INSTANCE}" \
    --vpc-id "${VPC_ID}" \
    --query 'GroupId' --output text)"
  log "Created Lambda SG ${LAMBDA_SG}"
fi

# Lambda → RDS: allow MySQL from Lambda SG on RDS SG
aws ec2 authorize-security-group-ingress \
  --profile "${PROFILE}" --region "${REGION}" \
  --group-id "${RDS_SG}" \
  --protocol tcp --port "${DB_PORT:-3306}" \
  --source-group "${LAMBDA_SG}" \
  --description "bittice flush-host-cache lambda" 2>/dev/null || true

BUILD_DIR="$(mktemp -d)"
trap 'rm -rf "${BUILD_DIR}"' EXIT
pip3 install -q pymysql -t "${BUILD_DIR}"
cp "${SCRIPT_DIR}/lambda_flush_host_cache.py" "${BUILD_DIR}/"
(
  cd "${BUILD_DIR}"
  zip -q -r9 "${BUILD_DIR}/function.zip" .
)

ROLE_NAME="${FUNCTION_NAME}-role"
TRUST='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
ROLE_ARN="$(aws iam get-role --profile "${PROFILE}" --role-name "${ROLE_NAME}" --query 'Role.Arn' --output text 2>/dev/null || true)"
if [[ -z "${ROLE_ARN}" || "${ROLE_ARN}" == "None" ]]; then
  ROLE_ARN="$(aws iam create-role \
    --profile "${PROFILE}" --role-name "${ROLE_NAME}" \
    --assume-role-policy-document "${TRUST}" \
    --query 'Role.Arn' --output text)"
  aws iam attach-role-policy \
    --profile "${PROFILE}" --role-name "${ROLE_NAME}" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole
  log "Waiting for IAM role propagation…"
  sleep 10
fi

ENV_JSON="$(python3 -c "import json,os; print(json.dumps({k:os.environ[k] for k in ('DB_HOST','DB_USER','DB_PASS','DB_PORT') if os.environ.get(k)}))")"
# Add FLUSH_SECRET via CLI below

if aws lambda get-function --profile "${PROFILE}" --region "${REGION}" \
  --function-name "${FUNCTION_NAME}" >/dev/null 2>&1; then
  log "Updating function ${FUNCTION_NAME}…"
  aws lambda update-function-code \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" \
    --zip-file "fileb://${BUILD_DIR}/function.zip" >/dev/null
  aws lambda update-function-configuration \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" \
    --environment "Variables={DB_HOST=${DB_HOST},DB_USER=${DB_USER},DB_PASS=${DB_PASS},DB_PORT=${DB_PORT:-3306},FLUSH_SECRET=${SECRET}}" \
    --timeout 30 >/dev/null
else
  log "Creating function ${FUNCTION_NAME}…"
  aws lambda create-function \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" \
    --runtime python3.11 \
    --role "${ROLE_ARN}" \
    --handler lambda_flush_host_cache.handler \
    --zip-file "fileb://${BUILD_DIR}/function.zip" \
    --timeout 30 \
    --memory-size 128 \
    --vpc-config "SubnetIds=${SUBNETS},SecurityGroupIds=${LAMBDA_SG}" \
    --environment "Variables={DB_HOST=${DB_HOST},DB_USER=${DB_USER},DB_PASS=${DB_PASS},DB_PORT=${DB_PORT:-3306},FLUSH_SECRET=${SECRET}}" >/dev/null
  log "Waiting for Lambda to become active…"
  aws lambda wait function-active-v2 \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" 2>/dev/null || sleep 15
fi

# Function URL (public + shared secret — only flush, no data exfiltration)
URL_CONFIG="$(aws lambda get-function-url-config \
  --profile "${PROFILE}" --region "${REGION}" \
  --function-name "${FUNCTION_NAME}" 2>/dev/null || true)"
if [[ -z "${URL_CONFIG}" ]]; then
  aws lambda create-function-url-config \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" \
    --auth-type NONE \
    --cors '{"AllowMethods":["POST"],"AllowOrigins":["*"]}' >/dev/null
  aws lambda add-permission \
    --profile "${PROFILE}" --region "${REGION}" \
    --function-name "${FUNCTION_NAME}" \
    --statement-id FunctionURLAllowPublic \
    --action lambda:InvokeFunctionUrl \
    --principal "*" \
    --function-url-auth-type NONE 2>/dev/null || true
fi

FLUSH_URL="$(aws lambda get-function-url-config \
  --profile "${PROFILE}" --region "${REGION}" \
  --function-name "${FUNCTION_NAME}" \
  --query 'FunctionUrl' --output text)"
FLUSH_URL="${FLUSH_URL%/}"

cat >"${OUT_ENV}" <<EOF
# Generated by setup-flush-lambda.sh — install on EC2: /opt/bittice/ops/flush-lambda.env
BITTICE_OPS_FLUSH_URL=${FLUSH_URL}
BITTICE_OPS_FLUSH_SECRET=${SECRET}
EOF
chmod 600 "${OUT_ENV}"

log "Wrote ${OUT_ENV}"
log "On EC2:"
echo "  scp ${OUT_ENV} ubuntu@<EC2>:/opt/bittice/ops/flush-lambda.env"
echo "  ssh ubuntu@<EC2> 'chmod 600 /opt/bittice/ops/flush-lambda.env'"
log "Also run: deploy/ops/ensure-rds-max-connect-errors.sh (prevention)"
