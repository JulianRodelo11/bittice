#!/usr/bin/env bash
# Derive and optionally apply ops-driven RAM/CDC/warm plan from .bittice_ops.json.
#
#   sudo ./ops-ram-plan.sh                  # dry-run
#   sudo ./ops-ram-plan.sh --apply          # dry-run + POST /_config/reload
#   sudo ./ops-ram-plan.sh --json
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"

exec python3 "${SCRIPT_DIR}/ops-ram-plan.py" --data-root "${DATA_ROOT}" "$@"
