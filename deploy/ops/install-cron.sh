#!/usr/bin/env bash
# Install / refresh cron on the current EC2 host (run via SSH as ubuntu).
set -euo pipefail

OPS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRON_LINE="*/5 * * * * ${OPS_DIR}/run-consistency-check.sh >> /var/log/bittice-consistency.log 2>&1"

( crontab -l 2>/dev/null | grep -v 'run-consistency-check.sh' || true
  echo "${CRON_LINE}"
) | crontab -

echo "Installed cron:"
crontab -l | grep consistency-check
