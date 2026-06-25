#!/usr/bin/env python3
"""Snapshot EC2 host resources for Bittice cloud ops."""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


def sh(cmd: str) -> str:
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else ""


def sh_lines(cmd: str) -> list[str]:
    out = sh(cmd)
    return [ln for ln in out.splitlines() if ln.strip()] if out else []


def imds(path: str) -> str:
    tok = sh(
        'curl -sf -X PUT "http://169.254.169.254/latest/api/token" '
        '-H "X-aws-ec2-metadata-token-ttl-seconds: 60"'
    )
    if not tok:
        return ""
    return sh(
        f'curl -sf -H "X-aws-ec2-metadata-token: {tok}" '
        f'"http://169.254.169.254/latest/meta-data/{path}"'
    )


def du_bytes(path: Path) -> int | None:
    if not path.exists():
        return None
    out = sh(f"du -sb {path} 2>/dev/null | awk '{{print $1}}'")
    if not out.isdigit():
        return None
    return int(out)


def fmt_bytes(n: int | None) -> str:
    if n is None:
        return "n/a"
    if n >= 1024**3:
        return f"{n / 1024**3:.2f} GiB"
    if n >= 1024**2:
        return f"{n / 1024**2:.1f} MiB"
    if n >= 1024:
        return f"{n / 1024:.0f} KiB"
    return f"{n} B"


def collect(data_root: Path) -> dict:
    warn: list[str] = []
    err: list[str] = []

    hostname = sh("hostname") or "?"
    instance_id = imds("instance-id") or "?"
    instance_type = imds("instance-type") or "?"
    uptime_line = sh("uptime -p 2>/dev/null") or sh("uptime | sed 's/.*up/up/'") or "?"

    ncpu = int(sh("nproc 2>/dev/null") or "1")
    load_parts = (sh("cat /proc/loadavg 2>/dev/null") or "0 0 0").split()[:3]
    load_1, load_5, load_15 = (float(x) for x in load_parts)
    load_pct = round(100.0 * load_1 / max(ncpu, 1), 1)

    mem_line = sh("free -m | awk '/^Mem:/ {print $2,$3,$7,$4}'")
    if mem_line:
        mem_total, mem_used, mem_avail, mem_free = (int(x) for x in mem_line.split())
    else:
        mem_total = mem_used = mem_avail = mem_free = 0
    mem_used_pct = round(100.0 * mem_used / mem_total, 1) if mem_total else 0
    mem_avail_pct = round(100.0 * mem_avail / mem_total, 1) if mem_total else 0

    df_root = sh("df -BM / | awk 'NR==2 {print $2,$3,$4,$5}'")
    if df_root:
        root_total_m, root_used_m, root_avail_m, root_use_pct_s = df_root.split()
        root_total_g = round(int(root_total_m.rstrip("M")) / 1024, 2)
        root_used_g = round(int(root_used_m.rstrip("M")) / 1024, 2)
        root_avail_g = round(int(root_avail_m.rstrip("M")) / 1024, 2)
        root_use_pct = int(root_use_pct_s.rstrip("%"))
    else:
        root_total_g = root_used_g = root_avail_g = 0.0
        root_use_pct = 0

    data_b = du_bytes(data_root)
    mirror_b = du_bytes(data_root / "mirror")
    profiles_b = du_bytes(data_root / "profiles")

    docker_rows = []
    for line in sh_lines(
        'docker stats --no-stream --format "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}" 2>/dev/null'
    ):
        parts = line.split("\t")
        if len(parts) < 4:
            continue
        docker_rows.append(
            {
                "name": parts[0],
                "cpu_percent": parts[1].rstrip("%"),
                "memory": parts[2],
                "memory_percent": parts[3].rstrip("%"),
            }
        )

    bittice_running = any(r["name"] == "bittice" for r in docker_rows)

    if root_use_pct >= 90:
        err.append(f"disk root {root_use_pct}% used")
    elif root_use_pct >= 80:
        warn.append(f"disk root {root_use_pct}% used")
    if mem_avail_pct <= 10:
        err.append(f"memory only {mem_avail_pct}% available")
    elif mem_avail_pct <= 20:
        warn.append(f"memory only {mem_avail_pct}% available")
    if load_1 > ncpu * 1.5:
        warn.append(f"load {load_1:.2f} on {ncpu} vCPU")
    if not bittice_running:
        warn.append("bittice container not running")

    return {
        "hostname": hostname,
        "instance_id": instance_id,
        "instance_type": instance_type,
        "uptime": uptime_line,
        "cpu": {
            "vcpus": ncpu,
            "load_1m": load_1,
            "load_5m": load_5,
            "load_15m": load_15,
            "load_pct_of_capacity": load_pct,
        },
        "memory_mib": {
            "total": mem_total,
            "used": mem_used,
            "available": mem_avail,
            "free": mem_free,
            "used_percent": mem_used_pct,
            "available_percent": mem_avail_pct,
        },
        "disk_root_gib": {
            "total": root_total_g,
            "used": root_used_g,
            "available": root_avail_g,
            "used_percent": root_use_pct,
        },
        "bittice_data": {
            "data_root": str(data_root),
            "total_bytes": data_b,
            "mirror_bytes": mirror_b,
            "profiles_bytes": profiles_b,
            "total_gib": round(data_b / (1024**3), 3) if data_b is not None else None,
            "mirror_gib": round(mirror_b / (1024**3), 3) if mirror_b is not None else None,
            "profiles_gib": round(profiles_b / (1024**3), 3) if profiles_b is not None else None,
        },
        "docker": docker_rows,
        "warnings": warn,
        "errors": err,
    }


def status_flag(pct_used: float, avail_pct: float | None = None) -> str:
    if avail_pct is not None and avail_pct <= 10:
        return "CRIT"
    if pct_used >= 90 or (avail_pct is not None and avail_pct <= 20):
        return "WARN"
    return "OK"


def print_table(report: dict) -> None:
    cpu = report["cpu"]
    mem = report["memory_mib"]
    disk = report["disk_root_gib"]
    data = report["bittice_data"]

    print(
        f"HOST          {report['hostname']}  {report['instance_id']}  "
        f"{report['instance_type']}  {report['uptime']}"
    )
    print(
        f"CPU           {cpu['vcpus']} vCPU   load {cpu['load_1m']:.2f} / "
        f"{cpu['load_5m']:.2f} / {cpu['load_15m']:.2f}   "
        f"(~{cpu['load_pct_of_capacity']}% of capacity)   "
        f"{status_flag(cpu['load_pct_of_capacity'])}"
    )
    print(
        f"MEMORY        {mem['total']} MiB total   {mem['used']} MiB used   "
        f"{mem['available']} MiB available   {mem['available_percent']}% free   "
        f"{status_flag(mem['used_percent'], mem['available_percent'])}"
    )
    print(
        f"DISK /        {disk['total']} GiB total   {disk['used']} GiB used   "
        f"{disk['available']} GiB free   {disk['used_percent']}% used   "
        f"{status_flag(disk['used_percent'])}"
    )
    if data["total_bytes"] is not None:
        extra = f"mirror {fmt_bytes(data['mirror_bytes'])}"
        if data["profiles_bytes"] is not None:
            extra += f", profiles {fmt_bytes(data['profiles_bytes'])}"
        print(f"DISK data     {data['data_root']}   {fmt_bytes(data['total_bytes'])} total ({extra})")

    print("DOCKER")
    if not report["docker"]:
        print("  (no running containers)")
    else:
        print(f"  {'NAME':<12} {'CPU':>8} {'MEMORY':<22} {'MEM%':>6}")
        for r in report["docker"]:
            print(
                f"  {r['name']:<12} {r['cpu_percent']:>7}% "
                f"{r['memory']:<22} {r['memory_percent']:>5}%"
            )

    if report["warnings"] or report["errors"]:
        print("")
        for w in report["warnings"]:
            print(f"WARN  {w}")
        for e in report["errors"]:
            print(f"CRIT  {e}")
    else:
        print("")
        print("All resource checks OK")


def main() -> int:
    parser = argparse.ArgumentParser(description="Bittice EC2 host resource snapshot")
    parser.add_argument(
        "--data-root",
        default="/opt/bittice/data",
        help="Bittice data directory (default: /opt/bittice/data)",
    )
    parser.add_argument("--json", action="store_true", help="JSON output")
    args = parser.parse_args()

    report = collect(Path(args.data_root))
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print_table(report)

    if report["errors"]:
        return 2
    if report["warnings"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
