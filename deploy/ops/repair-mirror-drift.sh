#!/usr/bin/env bash
# Re-bootstrap mirror table(s) whose live row count lags MySQL (persistent drift).
#
# Safe recovery path: invalidate CDC bootstrap state, delete on-disk mirror dirs,
# restart engine → CDC runs SELECT * snapshot for those tables again.
#
# Run AFTER deploying v0.1.162+ (reconcile_orphan_rows flush fix). Without that
# fix, UPDATE-heavy tables will drift again.
#
# On EC2 (local):
#   sudo ./repair-mirror-drift.sh --entity parking_host --yes \
#     --table attendant.pagos \
#     --table attendant.entradaVehiculos \
#     --table attendant.transacciones
#
# Dry run:
#   ./repair-mirror-drift.sh --entity parking_host --dry-run --table pagos
#
# From laptop (SSH):
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/repair-mirror-drift-cloud.sh --entity parking_host --yes
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
ENTITY=""
TABLES=()
DRY_RUN=0
YES=0
DIAGNOSE=0
SKIP_CHECK=0
CONTAINER="${BITTICE_CONTAINER:-bittice}"

usage() {
  cat <<'EOF'
Usage: repair-mirror-drift.sh --entity <profile> [options]

Required:
  --entity <profile>     Profile folder under data/profiles/ (e.g. parking_host).

Options:
  --table <filter>       Table filter (repeatable). Same rules as check-mirror:
                         pagos, attendant.pagos, db_attendant_prod.pagos, etc.
                         Default: attendant.pagos, attendant.entradaVehiculos,
                         attendant.transacciones (resolved against bootstrapped_tables).
  --dry-run              Print actions without modifying state or restarting.
  --yes                  Skip confirmation prompt.
  --diagnose             Run PK diff (diagnose-mirror-drift.py) before repair.
  --skip-check           Do not run check-mirror before/after repair.
  -h, --help             Show this help.

Environment:
  BITTICE_DATA_ROOT      Default /opt/bittice/data
  BITTICE_CONTAINER      Default bittice
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --entity) ENTITY="${2:?}"; shift 2 ;;
    --table) TABLES+=("${2:?}"); shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --yes) YES=1; shift ;;
    --diagnose) DIAGNOSE=1; shift ;;
    --skip-check) SKIP_CHECK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

if [[ -z "${ENTITY}" ]]; then
  echo "repair-mirror-drift: --entity is required" >&2
  usage
  exit 1
fi

if [[ ${#TABLES[@]} -eq 0 ]]; then
  TABLE_PATTERNS=(
    attendant.pagos
    attendant.entradaVehiculos
    attendant.transacciones
  )
else
  TABLE_PATTERNS=("${TABLES[@]}")
fi

PROFILE_DIR="${DATA_ROOT}/profiles/${ENTITY}"
STATE_PATH="${PROFILE_DIR}/cdc_state.json"
CFG_PATH="${PROFILE_DIR}/cdc_config.json"

if [[ ! -f "${STATE_PATH}" || ! -f "${CFG_PATH}" ]]; then
  echo "repair-mirror-drift: missing profile under ${PROFILE_DIR}" >&2
  exit 1
fi

TABLES=()
while IFS= read -r qkey; do
  [[ -n "${qkey}" ]] && TABLES+=("${qkey}")
done < <(
  python3 - "${STATE_PATH}" "${SCRIPT_DIR}" "${TABLE_PATTERNS[@]}" <<'PY'
import json, sys
from pathlib import Path

sys.path.insert(0, sys.argv[2])
from table_qkey import DEFAULT_ATTENDANT_DRIFT_PATTERNS, resolve_table_filters

state_path = Path(sys.argv[1])
patterns = sys.argv[3:] or list(DEFAULT_ATTENDANT_DRIFT_PATTERNS)
bootstrapped = json.loads(state_path.read_text()).get("bootstrapped_tables", [])
for q in resolve_table_filters(bootstrapped, patterns):
    print(q)
PY
)

if [[ ${#TABLES[@]} -eq 0 ]]; then
  echo "repair-mirror-drift: no bootstrapped tables matched filters: ${TABLE_PATTERNS[*]}" >&2
  exit 1
fi

log() { echo "[repair-mirror-drift] $*"; }

run_diagnose() {
  local py="${SCRIPT_DIR}/diagnose-mirror-drift.py"
  if [[ ! -f "${py}" ]]; then
    log "diagnose script not found at ${py}; skipping --diagnose"
    return 0
  fi
  local -a diag_args=(--profile "${ENTITY}")
  for t in "${TABLES[@]}"; do
    diag_args+=(--table "${t}")
  done
  log "Running PK diff diagnose…"
  if command -v python3 >/dev/null 2>&1 && python3 -c "import pyroaring, mysql.connector" 2>/dev/null; then
    BITTICE_DATA_ROOT="${DATA_ROOT}" python3 "${py}" "${diag_args[@]}" || true
    return 0
  fi
  local diag_cmd=""
  for t in "${TABLES[@]}"; do
    diag_cmd+=" --table $(printf '%q' "${t}")"
  done
  docker run --rm --network host \
    -v "${DATA_ROOT}:/data" \
    -v "${py}:/diag.py:ro" \
    python:3.11-slim bash -c \
    "pip install -q pyroaring mysql-connector-python && BITTICE_DATA_ROOT=/data python3 /diag.py --profile $(printf '%q' "${ENTITY}")${diag_cmd}" || true
}

run_check_mirror() {
  local when="${1:-}"
  local img
  img="$(docker inspect "${CONTAINER}" --format '{{.Config.Image}}' 2>/dev/null || true)"
  [[ -z "${img}" ]] && img="ghcr.io/julianrodelo11/bittice:stable"
  log "check-mirror (${when})…"
  local -a cmd=(check-mirror --entity "${ENTITY}" --revalidate)
  for t in "${TABLES[@]}"; do
    cmd+=(--table "${t}")
  done
  docker run --rm --network host \
    -v "${DATA_ROOT}:/app/data" \
    -e BITTICE_DATA_ROOT=/app/data \
    "${img}" \
    "${cmd[@]}"
}

resolve_mirror_dirs() {
  python3 - "${DATA_ROOT}" "${CFG_PATH}" "${TABLES[@]}" <<'PY'
import json, sys
from pathlib import Path

data_root = Path(sys.argv[1])
cfg = json.loads(Path(sys.argv[2]).read_text())
qkeys = sys.argv[3:]
sync_all = cfg.get("sync_all_databases", False)
database = cfg.get("database") or ""
entity = cfg.get("entity") or ""

def mirror_dir(qkey: str) -> Path:
    if sync_all and "." in qkey:
        schema, table = qkey.split(".", 1)
        disk_entity = schema.lower()
    else:
        disk_entity = entity or "default"
        table = qkey.split(".", 1)[-1]
    primary = data_root / "mirror" / disk_entity
    direct = primary / table
    if direct.is_dir():
        return direct
    if primary.is_dir():
        for e in primary.iterdir():
            if e.is_dir() and e.name.lower() == table.lower():
                return e
    return direct

for q in qkeys:
    print(f"{q}\t{mirror_dir(q)}")
PY
}

[[ "${DIAGNOSE}" -eq 1 ]] && run_diagnose

if [[ "${SKIP_CHECK}" -eq 0 && "${DRY_RUN}" -eq 0 ]]; then
  if ! run_check_mirror "before"; then
    log "Drift detected (expected). Proceeding with re-bootstrap…"
  fi
fi

log "Tables to re-bootstrap:"
while IFS=$'\t' read -r qkey mdir; do
  echo "  ${qkey}  →  ${mdir}"
done < <(resolve_mirror_dirs)

if [[ "${DRY_RUN}" -eq 1 ]]; then
  log "Dry run — no changes made."
  exit 0
fi

if [[ "${YES}" -eq 0 ]]; then
  echo
  read -r -p "Stop ${CONTAINER}, delete mirror dirs, and re-bootstrap ${#TABLES[@]} table(s)? [y/N] " ans
  [[ "${ans}" == [yY] || "${ans}" == [yY][eE][sS] ]] || { log "Aborted."; exit 1; }
fi

log "Stopping ${CONTAINER}…"
docker stop "${CONTAINER}" >/dev/null

python3 - "${STATE_PATH}" "${TABLES[@]}" <<'PY'
import json, sys
from pathlib import Path

path = Path(sys.argv[1])
tables = sys.argv[2:]
state = json.loads(path.read_text())
boot = state.get("bootstrapped_tables", [])
pk_map = state.get("pk_map", {})
removed = []
for q in tables:
    if q in boot:
        boot = [x for x in boot if x != q]
        removed.append(q)
    pk_map.pop(q, None)
if not removed:
    print("WARN: none of the requested tables were in bootstrapped_tables", file=sys.stderr)
state["bootstrapped_tables"] = boot
state["pk_map"] = pk_map
path.write_text(json.dumps(state, indent=2) + "\n")
print("Updated", path, "removed:", ", ".join(tables))
PY

while IFS=$'\t' read -r qkey mdir; do
  if [[ -d "${mdir}" ]]; then
    log "Removing mirror dir ${mdir}"
    rm -rf "${mdir}"
  else
    log "Mirror dir already absent: ${mdir}"
  fi
done < <(resolve_mirror_dirs)

log "Starting ${CONTAINER}…"
docker start "${CONTAINER}" >/dev/null

log "Waiting for CDC bootstrap (up to 120s)…"
deadline=$((SECONDS + 120))
while [[ "${SECONDS}" -lt "${deadline}" ]]; do
  if docker logs "${CONTAINER}" 2>&1 | tail -30 | grep -qE "Bootstrap.*complete|CDC: bootstrap.*finished|bootstrapped ${#TABLES[@]}"; then
    break
  fi
  sleep 5
done

log "Tail engine log:"
docker logs "${CONTAINER}" 2>&1 | tail -15

if [[ "${SKIP_CHECK}" -eq 0 ]]; then
  log "Re-run check-mirror in 10s…"
  sleep 10
  if run_check_mirror "after"; then
    log "OK — selected tables match MySQL."
  else
    log "WARN — drift may remain until bootstrap completes; re-run check-mirror-cloud.sh --revalidate"
    exit 1
  fi
fi

log "Done."
