#!/usr/bin/env python3
"""Compare MySQL PKs vs mirror live rows for drift diagnosis (run on EC2 or via SSH).

Requires: pip install pyroaring mysql-connector-python (or run inside python:3.11-slim docker).
"""
from __future__ import annotations

import argparse
import json
import os
import struct
import sys
from pathlib import Path

try:
    import mysql.connector
    import pyroaring
except ImportError as e:
    print(f"Missing dependency: {e}", file=sys.stderr)
    print("Install: pip install pyroaring mysql-connector-python", file=sys.stderr)
    sys.exit(2)


def mysql_pks(conn, schema: str, table: str, pk: str) -> set[str]:
    cur = conn.cursor()
    cur.execute(f"SELECT CAST(`{pk}` AS CHAR) FROM `{schema}`.`{table}` ORDER BY 1")
    return {str(r[0]) for r in cur.fetchall()}


def mirror_live_pks(table_dir: Path, pk: str) -> tuple[list[str], int]:
    live: list[str] = []
    empty = 0
    segs_dir = table_dir / "segments"
    if not segs_dir.is_dir():
        return live, empty

    for segdir in sorted(segs_dir.iterdir()):
        if not segdir.is_dir():
            continue
        off_path = segdir / f"{pk}.offsets"
        dat_path = segdir / f"{pk}.dat"
        if not off_path.is_file() or not dat_path.is_file():
            continue

        off = off_path.read_bytes()
        n = len(off) // 8
        deleted: set[int] = set()
        del_path = segdir / "deleted.bitmap"
        if del_path.is_file():
            deleted = set(pyroaring.BitMap.deserialize(del_path.read_bytes()))

        dat = dat_path.read_bytes()
        for i in range(n):
            if i in deleted:
                continue
            start = struct.unpack_from("<Q", off, i * 8)[0]
            if start + 8 > len(dat):
                empty += 1
                continue
            ln = struct.unpack_from("<Q", dat, start)[0]
            end = start + 8 + ln
            if end > len(dat):
                empty += 1
                continue
            val = dat[start + 8 : end].decode("utf-8", errors="replace")
            if not val:
                empty += 1
            live.append(val)

    return live, empty


def pk_from_manifest(table_dir: Path) -> str:
    manifest = json.loads((table_dir / "manifest.json").read_text())
    pk = manifest.get("primary_key") or ""
    if not pk and manifest.get("primary_key_columns"):
        pk = manifest["primary_key_columns"][0]
    if not pk:
        raise ValueError(f"No primary_key in {table_dir / 'manifest.json'}")
    return pk


def parse_qkey(qkey: str) -> tuple[str, str]:
    if "." not in qkey:
        raise ValueError(f"Expected schema.table, got {qkey!r}")
    schema, table = qkey.split(".", 1)
    return schema, table


def main() -> int:
    parser = argparse.ArgumentParser(description="Diagnose mirror drift by PK diff")
    parser.add_argument("--data-root", default=os.environ.get("BITTICE_DATA_ROOT", "/opt/bittice/data"))
    parser.add_argument("--profile", default="bittice_host")
    parser.add_argument("--table", action="append", dest="tables", help="schema.table (repeatable)")
    args = parser.parse_args()

    data_root = Path(args.data_root)
    cfg_path = data_root / "profiles" / args.profile / "cdc_config.json"
    cfg = json.loads(cfg_path.read_text())

    conn = mysql.connector.connect(
        host=cfg["host"],
        port=int(cfg.get("port", 3306)),
        user=cfg["user"],
        password=cfg.get("pass") or cfg.get("password"),
    )

    state_path = data_root / "profiles" / args.profile / "cdc_state.json"
    bootstrapped = json.loads(state_path.read_text()).get("bootstrapped_tables", [])
    targets = args.tables or [
        t
        for t in bootstrapped
        if t.startswith("db_attendant_dev.")
        and t.split(".", 1)[1] in ("pagos", "entradaVehiculos", "transacciones")
    ]
    if not targets:
        targets = bootstrapped

    mirror_root = data_root / "mirror"
    exit_code = 0

    for qkey in sorted(targets):
        schema, table = parse_qkey(qkey)
        entity = schema.lower() if cfg.get("sync_all_databases") else cfg.get("entity", args.profile)
        table_dir = mirror_root / entity / table
        if not table_dir.is_dir():
            # case-insensitive fallback
            parent = mirror_root / entity
            if parent.is_dir():
                for e in parent.iterdir():
                    if e.is_dir() and e.name.lower() == table.lower():
                        table_dir = e
                        break

        if not table_dir.is_dir():
            print(f"=== {qkey} === mirror dir not found: {table_dir}")
            exit_code = 1
            continue

        pk = pk_from_manifest(table_dir)
        src = mysql_pks(conn, schema, table, pk)
        live, empty_pk = mirror_live_pks(table_dir, pk)
        uniq = set(live)
        missing = sorted(src - uniq, key=lambda x: int(x) if x.isdigit() else x)
        extra = sorted(uniq - src, key=lambda x: int(x) if x.isdigit() else x)
        diff = len(src) - len(live)

        print(f"=== {qkey} (pk={pk}) ===")
        print(f"  mysql_rows={len(src)}  mirror_live_physical={len(live)}  diff={diff}")
        print(f"  mirror_unique_pk={len(uniq)}  unreadable_pk_rows={empty_pk}")
        print(f"  missing_in_mirror ({len(missing)}): {missing}")
        print(f"  extra_in_mirror ({len(extra)}): {extra}")
        if missing or extra or empty_pk:
            exit_code = 1
        print()

    conn.close()
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
