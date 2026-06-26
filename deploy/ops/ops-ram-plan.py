#!/usr/bin/env python3
"""Derive RAM/cache/CDC plan from .bittice_ops.json (saved queries contract).

Compares ops-referenced mirror tables with CDC bootstrapped tables, and lists
warm targets (filter fields P0 + join/order extended P1) matching engine warm.rs.

Usage:
  BITTICE_DATA_ROOT=/opt/bittice/data ./ops-ram-plan.py
  ./ops-ram-plan.py --apply   # POST /_config/reload on admin port (localhost)
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path
from typing import Any

DEFAULT_DATA_ROOT = Path("/opt/bittice/data")
OPS_FILE = ".bittice_ops.json"
ADMIN_RELOAD = "http://127.0.0.1:8080/_config/reload"


def load_ops_raw(path: Path, include_internal: bool) -> list[dict[str, Any]]:
    raw = json.loads(path.read_text())
    if not isinstance(raw, list):
        raise SystemExit(f"expected JSON array in {path}")
    out: list[dict[str, Any]] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        details = item.get("details") if item.get("type") else item
        if not isinstance(details, dict):
            continue
        name = details.get("name") or item.get("name") or ""
        if not include_internal and str(name).startswith("__internal_"):
            continue
        if item.get("type"):
            out.append(item)
        else:
            out.append({"type": "read", "details": item})
    return out


def read_ops_details(raw_ops: list[dict[str, Any]]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for item in raw_ops:
        if item.get("type") == "read":
            d = item.get("details")
            if isinstance(d, dict):
                out.append(d)
    return out


def table_key(entity: str, table: str) -> str:
    return f"{entity.strip()}/{table.strip()}"


def split_alias_field(value: str, base_alias: str) -> tuple[str, str] | None:
    trimmed = (value or "").strip()
    if not trimmed or trimmed == "*":
        return None
    if "." in trimmed:
        alias, field = trimmed.split(".", 1)
        alias, field = alias.strip(), field.strip()
        if alias and field:
            return alias, field
        return None
    return base_alias, trimmed


def resolve_field_table(
    q: dict[str, Any], base_alias: str, field_ref: str
) -> tuple[str, str, str] | None:
    parsed = split_alias_field(field_ref, base_alias)
    if not parsed:
        return None
    alias, field = parsed
    if alias == base_alias:
        return q["entity"], q["table"], field
    for join in q.get("joins") or []:
        ja = (join.get("alias") or join.get("table") or "").strip()
        if ja == alias:
            ent = (join.get("entity") or q["entity"]).strip()
            return ent, join["table"], field
    return None


def collect_read_query_tables(q: dict[str, Any], out: set[str]) -> None:
    out.add(table_key(q["entity"], q["table"]))
    for join in q.get("joins") or []:
        ent = (join.get("entity") or q["entity"]).strip()
        out.add(table_key(ent, join["table"]))
    auth = q.get("auth_config") or {}
    if auth.get("enabled"):
        out.add(table_key(q["entity"], auth["table"]))
    profile = q.get("execution_profile")
    if isinstance(profile, dict) and profile.get("mode") == "split_enrichment":
        eq = profile.get("enrichment_query")
        if isinstance(eq, dict):
            collect_read_query_tables(eq, out)


def collect_op_tables(
    op: dict[str, Any],
    by_name: dict[str, dict[str, Any]],
    batch_visited: set[str],
    out: set[str],
) -> None:
    op_type = op.get("type")
    if op_type == "read" or ("entity" in op and "table" in op and "name" in op):
        q = op.get("details") if op_type else op
        if isinstance(q, dict):
            collect_read_query_tables(q, out)
        return
    if op_type == "insert":
        d = op.get("details") or op
        out.add(table_key(d["entity"], d["table"]))
    elif op_type == "update":
        d = op.get("details") or op
        out.add(table_key(d["entity"], d["table"]))
    elif op_type == "delete":
        d = op.get("details") or op
        out.add(table_key(d["entity"], d["table"]))
    elif op_type == "batch":
        d = op.get("details") or op
        name = d.get("name", "")
        if name in batch_visited:
            return
        batch_visited.add(name)
        for sub in d.get("operations") or []:
            sub_op = by_name.get(sub)
            if sub_op:
                collect_op_tables(sub_op, by_name, batch_visited, out)


def collect_ops_table_keys(raw_ops: list[dict[str, Any]]) -> set[str]:
    by_name: dict[str, dict[str, Any]] = {}
    for op in raw_ops:
        d = op.get("details") or op
        name = d.get("name") if isinstance(d, dict) else None
        if name:
            by_name[name] = op
    out: set[str] = set()
    batch_visited: set[str] = set()
    for op in raw_ops:
        collect_op_tables(op, by_name, batch_visited, out)
    return out


def collect_warm_plans(ops_details: list[dict[str, Any]]) -> tuple[dict, dict]:
    filter_targets: dict[tuple[str, str], set[str]] = defaultdict(set)
    extended_targets: dict[tuple[str, str], set[str]] = defaultdict(set)

    def add_filter(q: dict[str, Any]) -> None:
        base_alias = (q.get("table_alias") or q["table"]).strip()
        for f in q.get("filters") or []:
            field = f.get("field")
            if not field or field == "?":
                continue
            resolved = resolve_field_table(q, base_alias, field)
            if resolved:
                ent, tbl, col = resolved
                filter_targets[(ent, tbl)].add(col)

    def add_extended(q: dict[str, Any]) -> None:
        base_alias = (q.get("table_alias") or q["table"]).strip()
        for f in q.get("selected_fields") or []:
            if f == "*":
                continue
            parsed = split_alias_field(f, base_alias)
            if parsed and parsed[0] == base_alias:
                extended_targets[(q["entity"], q["table"])].add(parsed[1])
        for s in q.get("select") or []:
            parsed = split_alias_field(s.get("field") or "", base_alias)
            if parsed and parsed[0] == base_alias:
                extended_targets[(q["entity"], q["table"])].add(parsed[1])
        for o in q.get("order_by") or []:
            resolved = resolve_field_table(q, base_alias, o.get("field") or "")
            if resolved:
                ent, tbl, col = resolved
                extended_targets[(ent, tbl)].add(col)
        for join in q.get("joins") or []:
            ja = (join.get("alias") or join.get("table") or "").strip()
            je = (join.get("entity") or q["entity"]).strip()
            jt = join["table"]
            for cond in join.get("on") or []:
                for side in (cond.get("left"), cond.get("right")):
                    parsed = split_alias_field(side or "", base_alias)
                    if parsed and parsed[0] == ja:
                        extended_targets[(je, jt)].add(parsed[1])

    def walk(q: dict[str, Any]) -> None:
        add_filter(q)
        add_extended(q)
        profile = q.get("execution_profile")
        if isinstance(profile, dict) and profile.get("mode") == "split_enrichment":
            eq = profile.get("enrichment_query")
            if isinstance(eq, dict):
                walk(eq)

    for op in ops_details:
        if op.get("entity") and op.get("table"):
            walk(op)

    return dict(filter_targets), dict(extended_targets)


def bootstrapped_keys(data_root: Path) -> dict[str, set[str]]:
    """profile -> set of entity/table keys from cdc_state bootstrapped_tables."""
    out: dict[str, set[str]] = {}
    profiles = data_root / "profiles"
    if not profiles.is_dir():
        return out
    for state_path in profiles.glob("*/cdc_state.json"):
        profile = state_path.parent.name
        try:
            state = json.loads(state_path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        keys: set[str] = set()
        for qkey in state.get("bootstrapped_tables") or []:
            if "." in qkey:
                schema, table = qkey.rsplit(".", 1)
                keys.add(table_key(schema, table))
        out[profile] = keys
    return out


def cdc_config_tables(data_root: Path) -> dict[str, set[str]]:
    out: dict[str, set[str]] = {}
    profiles = data_root / "profiles"
    if not profiles.is_dir():
        return out
    for cfg_path in profiles.glob("*/cdc_config.json"):
        profile = cfg_path.parent.name
        try:
            cfg = json.loads(cfg_path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        keys: set[str] = set()
        entity = (cfg.get("entity") or profile).strip()
        sync_all = cfg.get("sync_all_databases", False)
        scoped = cfg.get("scoped_sync") or {}
        for db_name, tables in scoped.items():
            if not isinstance(tables, list):
                continue
            for t in tables:
                if sync_all:
                    keys.add(table_key(db_name, str(t)))
                else:
                    keys.add(table_key(entity, str(t)))
        for t in cfg.get("tables") or []:
            keys.add(table_key(entity, str(t)))
        out[profile] = keys
    return out


def qkey_to_key(qkey: str) -> str:
    if "." in qkey:
        s, t = qkey.rsplit(".", 1)
        return table_key(s, t)
    return qkey


def post_reload(url: str) -> dict[str, Any]:
    req = urllib.request.Request(url, method="POST", data=b"")
    req.add_header("Content-Length", "0")
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read().decode())


def format_warm_section(title: str, targets: dict) -> list[str]:
    lines = [title]
    for (ent, tbl) in sorted(targets.keys()):
        fields = ", ".join(sorted(targets[(ent, tbl)]))
        lines.append(f"  {ent}/{tbl}: {fields}")
    if len(lines) == 1:
        lines.append("  (none)")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description="Ops-driven RAM/CDC/warm plan")
    parser.add_argument(
        "--data-root",
        type=Path,
        default=Path(
            __import__("os").environ.get("BITTICE_DATA_ROOT", str(DEFAULT_DATA_ROOT))
        ),
    )
    parser.add_argument("--include-internal", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--apply",
        action="store_true",
        help="POST /_config/reload (refresh priority keys + ops cache after engine update)",
    )
    parser.add_argument(
        "--reload-url",
        default=ADMIN_RELOAD,
        help="Admin reload URL (default localhost:8080)",
    )
    args = parser.parse_args()

    data_root = args.data_root
    ops_path = data_root / OPS_FILE
    if not ops_path.is_file():
        print(f"ops-ram-plan: missing {ops_path}", file=sys.stderr)
        return 1

    raw_ops = load_ops_raw(ops_path, args.include_internal)
    ops_details = read_ops_details(raw_ops)
    ops_tables = collect_ops_table_keys(raw_ops)
    filter_warm, extended_warm = collect_warm_plans(ops_details)
    bootstrapped = bootstrapped_keys(data_root)
    all_bootstrapped = set().union(*bootstrapped.values()) if bootstrapped else set()
    cdc_tables = cdc_config_tables(data_root)
    all_cdc_config = set().union(*cdc_tables.values()) if cdc_tables else set()

    missing_mirror = sorted(ops_tables - all_bootstrapped)
    extra_mirror = sorted(all_bootstrapped - ops_tables)
    missing_vs_config = sorted(ops_tables - all_cdc_config) if all_cdc_config else []

    report = {
        "ops_path": str(ops_path),
        "ops_count": len(ops_details),
        "ops_table_count": len(ops_tables),
        "ops_tables": sorted(ops_tables),
        "bootstrapped_by_profile": {k: sorted(v) for k, v in bootstrapped.items()},
        "missing_in_cdc_mirror": missing_mirror,
        "extra_in_cdc_mirror": extra_mirror,
        "cdc_config_tables": {k: sorted(v) for k, v in cdc_tables.items()},
        "missing_vs_cdc_config": missing_vs_config,
        "warm_p0_filter_fields": {
            f"{e}/{t}": sorted(fields) for (e, t), fields in filter_warm.items()
        },
        "warm_p1_extended_fields": {
            f"{e}/{t}": sorted(fields) for (e, t), fields in extended_warm.items()
        },
        "recommendations": [],
    }

    if missing_mirror:
        report["recommendations"].append(
            "CDC mirror missing ops tables — bootstrap or repair before relying on queries."
        )
    if extra_mirror:
        report["recommendations"].append(
            "CDC mirrors tables not referenced by ops — safe to keep; enable "
            "BITTICE_CDC_SYNC_ONLY_OPS=1 after confirming missing_in_cdc_mirror is empty."
        )
    if not missing_mirror and extra_mirror:
        report["recommendations"].append(
            "Ready for BITTICE_CDC_SYNC_ONLY_OPS=1 trial: all ops tables are bootstrapped."
        )

    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(f"[ops-ram-plan] {ops_path}")
        print(f"  Saved ops (public): {len(ops_details)}")
        print(f"  Tables referenced:  {len(ops_tables)}")
        print("")
        print("Tables from ops:")
        for k in sorted(ops_tables):
            print(f"  {k}")
        print("")
        for profile, keys in sorted(bootstrapped.items()):
            print(f"CDC bootstrapped ({profile}): {len(keys)} tables")
        if missing_mirror:
            print("")
            print("MISSING in CDC mirror (ops need these):")
            for k in missing_mirror:
                print(f"  ! {k}")
        if extra_mirror:
            print("")
            print("Extra in CDC mirror (not in ops — candidate to drop with SYNC_ONLY_OPS):")
            for k in extra_mirror:
                print(f"  - {k}")
        print("")
        print("\n".join(format_warm_section("Warm P0 (filter fields):", filter_warm)))
        print("")
        print("\n".join(format_warm_section("Warm P1 (join/order extended):", extended_warm)))
        if report["recommendations"]:
            print("")
            print("Recommendations:")
            for r in report["recommendations"]:
                print(f"  • {r}")

    if args.apply:
        try:
            result = post_reload(args.reload_url)
            print(f"\n[ops-ram-plan] reload OK: {result}")
        except urllib.error.URLError as e:
            print(f"\n[ops-ram-plan] reload failed: {e}", file=sys.stderr)
            return 1

    return 1 if missing_mirror else 0


if __name__ == "__main__":
    sys.exit(main())
