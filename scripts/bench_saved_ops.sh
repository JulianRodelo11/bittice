#!/usr/bin/env bash
# Benchmark saved REST ops — extracts meta.engine_time_ms and debug_info.
#
# Usage:
#   ./scripts/bench_saved_ops.sh
#   BASE_URL=http://127.0.0.1:3000 RUNS=10 ./scripts/bench_saved_ops.sh
#   BITTICE_MAX_OPEN_TABLES=3 ./scripts/bench_saved_ops.sh   # stress eviction (set on engine, not here)
#
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:3000}"
RUNS="${RUNS:-5}"

endpoints=(
  "beparking-info-user-document?cedula=1032501364&tipoDocumento=1"
  "beparking-transaction-plate?placa=LCL979"
)

bench_one() {
  local path="$1"
  local url="${BASE_URL}/${path}"
  echo "=== ${path} (${RUNS} runs) ==="
  python3 - "$url" "$RUNS" <<'PY'
import json, sys, urllib.request, statistics

url, runs = sys.argv[1], int(sys.argv[2])
times = []
debug_last = ""
for i in range(runs):
    with urllib.request.urlopen(url, timeout=120) as resp:
        data = json.load(resp)
    meta = data.get("meta") or {}
    t = meta.get("engine_time_ms")
    if t is not None:
        times.append(float(t))
    debug_last = meta.get("debug_info") or debug_last

if not times:
    print("  (no engine_time_ms in response)")
    sys.exit(1)

times.sort()
p50 = times[len(times) // 2]
p95 = times[max(0, int(len(times) * 0.95) - 1)]
print(f"  min={times[0]:.2f}ms  p50={p50:.2f}ms  p95={p95:.2f}ms  max={times[-1]:.2f}ms")
print(f"  last debug: {debug_last[:160]}")
PY
  echo
}

echo "bench_saved_ops base=${BASE_URL} runs=${RUNS}"
echo

for ep in "${endpoints[@]}"; do
  bench_one "$ep"
done
