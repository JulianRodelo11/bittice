#!/usr/bin/env bash
# Install / refresh cron on the current EC2 host (run via SSH as ubuntu).
set -euo pipefail

OPS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_FILE="${CONSISTENCY_LOG:-${OPS_DIR}/consistency.log}"
CRON_LINE="*/5 * * * * ${OPS_DIR}/run-consistency-check.sh >> ${LOG_FILE} 2>&1"

touch "${LOG_FILE}" 2>/dev/null || true
chmod 600 "${LOG_FILE}" 2>/dev/null || true

( crontab -l 2>/dev/null | grep -v 'run-consistency-check.sh' || true
  echo "${CRON_LINE}"
) | crontab -

echo "Installed cron (log: ${LOG_FILE}):"
crontab -l | grep consistency-check
