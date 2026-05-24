#!/usr/bin/env bash
# Raise RDS max_connect_errors so transient TLS/handshake failures on the EC2 IP
# do not trigger error 1129. Run once from your laptop (AWS profile with RDS perms).
#
# Usage:
#   AWS_PROFILE=bittice deploy/ops/ensure-rds-max-connect-errors.sh
#   AWS_PROFILE=bittice deploy/ops/ensure-rds-max-connect-errors.sh bittice
set -euo pipefail

INSTANCE_ID="${1:-bittice}"
PROFILE="${AWS_PROFILE:-bittice}"
REGION="${AWS_REGION:-us-east-1}"
VALUE="${MAX_CONNECT_ERRORS:-1000000}"

log() { echo "[ensure-rds] $*"; }

PARAM_GROUP="$(aws rds describe-db-instances \
  --profile "${PROFILE}" --region "${REGION}" \
  --db-instance-identifier "${INSTANCE_ID}" \
  --query 'DBInstances[0].DBParameterGroups[0].DBParameterGroupName' \
  --output text)"

PARAM_FAMILY="$(aws rds describe-db-parameter-groups \
  --profile "${PROFILE}" --region "${REGION}" \
  --db-parameter-group-name "${PARAM_GROUP}" \
  --query 'DBParameterGroups[0].DBParameterGroupFamily' --output text)"

log "Instance ${INSTANCE_ID} → parameter group ${PARAM_GROUP} (family ${PARAM_FAMILY})"

if [[ "${PARAM_GROUP}" == default.* ]]; then
  CUSTOM_PG="${CUSTOM_PARAMETER_GROUP:-bittice-mysql-params}"
  log "Default parameter group cannot be edited — creating ${CUSTOM_PG}…"
  if ! aws rds describe-db-parameter-groups \
    --profile "${PROFILE}" --region "${REGION}" \
    --db-parameter-group-name "${CUSTOM_PG}" >/dev/null 2>&1; then
    aws rds create-db-parameter-group \
      --profile "${PROFILE}" --region "${REGION}" \
      --db-parameter-group-name "${CUSTOM_PG}" \
      --db-parameter-group-family "${PARAM_FAMILY}" \
      --description "Bittice fleet — raised max_connect_errors"
  fi
  PARAM_GROUP="${CUSTOM_PG}"
  log "Attaching ${PARAM_GROUP} to ${INSTANCE_ID} (may reboot)…"
  aws rds modify-db-instance \
    --profile "${PROFILE}" --region "${REGION}" \
    --db-instance-identifier "${INSTANCE_ID}" \
    --db-parameter-group-name "${PARAM_GROUP}" \
    --apply-immediately
  log "Waiting for instance to be available…"
  aws rds wait db-instance-available \
    --profile "${PROFILE}" --region "${REGION}" \
    --db-instance-identifier "${INSTANCE_ID}"
fi

CURRENT="$(aws rds describe-db-parameters \
  --profile "${PROFILE}" --region "${REGION}" \
  --db-parameter-group-name "${PARAM_GROUP}" \
  --query "Parameters[?ParameterName=='max_connect_errors'].ParameterValue | [0]" \
  --output text 2>/dev/null | head -1)"

if [[ "${CURRENT}" == "${VALUE}" ]]; then
  log "max_connect_errors already ${VALUE} — nothing to do."
  exit 0
fi

log "Setting max_connect_errors=${VALUE} (was: ${CURRENT:-unknown})…"
aws rds modify-db-parameter-group \
  --profile "${PROFILE}" --region "${REGION}" \
  --db-parameter-group-name "${PARAM_GROUP}" \
  --parameters "ParameterName=max_connect_errors,ParameterValue=${VALUE},ApplyMethod=immediate"

log "Done. If the parameter was static, reboot the RDS instance when convenient."
