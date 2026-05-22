#!/usr/bin/env bash
# Clear MySQL host_cache so the EC2 consistency cron can connect again.
#
# Must run from a host that is NOT blocked (your laptop with bittice-db/.env,
# or a GitHub Action with DB secrets). The blocked EC2 cannot flush itself.
#
# Usage (from repo root):
#   bittice-db/.env loaded:
#     deploy/ops/flush-mysql-host-cache.sh
#
# Or explicit:
#   DB_HOST=... DB_USER=... DB_PASS=... deploy/ops/flush-mysql-host-cache.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ENV_FILE="${BITTICE_DB_ENV:-${REPO_ROOT}/../bittice-db/.env}"

if [[ -f "${ENV_FILE}" ]]; then
  set -a
  # shellcheck source=/dev/null
  source "${ENV_FILE}"
  set +a
fi

HOST="${DB_HOST:?DB_HOST required}"
PORT="${DB_PORT:-3306}"
USER="${DB_USER:?DB_USER required}"
PASS="${DB_PASS:?DB_PASS required}"

log() { echo "[flush-host-cache] $*"; }

if command -v python3 >/dev/null 2>&1; then
  log "Flushing performance_schema.host_cache on ${HOST}…"
  python3 <<PY
import os, pymysql
conn = pymysql.connect(
    host=os.environ["DB_HOST"],
    port=int(os.environ.get("DB_PORT", "3306")),
    user=os.environ["DB_USER"],
    password=os.environ["DB_PASS"],
    ssl={"ssl": {}},
)
with conn.cursor() as cur:
    cur.execute("TRUNCATE TABLE performance_schema.host_cache")
conn.commit()
conn.close()
print("OK: host_cache truncated")
PY
  exit 0
fi

if command -v mysql >/dev/null 2>&1; then
  log "Flushing via mysql CLI…"
  mysql -h "${HOST}" -P "${PORT}" -u "${USER}" -p"${PASS}" --ssl-mode=REQUIRED \
    -e "TRUNCATE TABLE performance_schema.host_cache;"
  exit 0
fi

echo "Need python3+pymysql or mysql client." >&2
exit 1
