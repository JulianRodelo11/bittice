#!/usr/bin/env python3
"""
Bittice fleet ops — mirror vs source row counts → control plane.

NOT part of the open-source motor. Install only on *your* managed EC2 hosts
(see README.md). Reads CDC profile/state from the host data dir, counts MySQL
rows and mirror manifest rows, POSTs to POST /v1/health/consistency-check.

Required env (usually from docker inspect → runtime.env):
  BITTICE_DEPLOYMENT_ID, BITTICE_INSTANCE_TOKEN, BITTICE_CONTROL_PLANE_URL
Optional:
  BITTICE_DATA_ROOT  (default: /opt/bittice/data)
  CONSISTENCY_CHECK_DRY_RUN=1  — print payload, do not POST
"""
from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import pymysql
except ImportError:
    print("Missing pymysql. Run: pip3 install -r requirements.txt", file=sys.stderr)
    sys.exit(1)


def _env(name: str, default: str | None = None) -> str | None:
    v = os.environ.get(name, default)
    if v is None:
        return None
    v = v.strip()
    return v if v else None


def _mysql_ident(ident: str) -> str:
    return ident.replace("`", "``")


def _data_root() -> Path:
    return Path(_env("BITTICE_DATA_ROOT", "/opt/bittice/data") or "/opt/bittice/data")


def _profiles_root(root: Path) -> Path:
    return root / "profiles"


def _mirror_entity_dir(root: Path, entity: str) -> Path:
    new_path = root / "mirror" / entity
    if new_path.is_dir():
        return new_path
    legacy = root / entity
    if legacy.is_dir() and not (legacy / "cdc_config.json").is_file():
        return legacy
    return new_path


def _resolve_mirror_table_dir(entity_dir: Path, table: str) -> str:
    direct = entity_dir / table
    if direct.is_dir():
        return table
    want = table.lower()
    if entity_dir.is_dir():
        for child in entity_dir.iterdir():
            if child.is_dir() and child.name.lower() == want:
                return child.name
    return table


def _deleted_bitmap_len(seg_dir: Path) -> int:
    path = seg_dir / "deleted.bitmap"
    if not path.is_file():
        return 0
    try:
        import roaring  # type: ignore

        data = path.read_bytes()
        return len(roaring.Bitmap.deserialize(data))
    except Exception:
        # Fallback: tiny files ≈ one tombstone; avoid counting full record_count.
        return 1 if path.stat().st_size > 0 else 0


def _mirror_live_count(table_dir: Path) -> int:
    """Match engine query semantics: record_count minus on-disk tombstones per segment."""
    manifest_path = table_dir / "manifest.json"
    if not manifest_path.is_file():
        return 0
    with manifest_path.open(encoding="utf-8") as f:
        manifest: dict[str, Any] = json.load(f)
    segments_root = table_dir / "segments"
    live = 0
    for seg in manifest.get("segments") or []:
        rc = int(seg.get("record_count") or 0)
        seg_id = int(seg.get("id", 0))
        seg_dir = segments_root / f"seg_{seg_id:04}"
        dc = _deleted_bitmap_len(seg_dir) if seg_dir.is_dir() else int(seg.get("deleted_count") or 0)
        live += max(0, rc - dc)
    return live


def _parse_qkey(sync_all: bool, database: str, qkey: str) -> tuple[str, str]:
    if sync_all:
        if "." not in qkey:
            raise ValueError(f"malformed sync-all qkey: {qkey!r}")
        schema, table = qkey.split(".", 1)
        return schema, table
    return database, qkey


def _mysql_count(
    config: dict[str, Any],
    sync_all: bool,
    database: str,
    schema: str,
    table: str,
) -> int:
    host = config["host"]
    port = int(config.get("port") or 3306)
    user = config["user"]
    password = config.get("pass") or config.get("password") or ""
    conn = pymysql.connect(
        host=host,
        port=port,
        user=user,
        password=password,
        database=None if sync_all else database,
        charset="utf8mb4",
        connect_timeout=15,
        read_timeout=60,
    )
    try:
        with conn.cursor() as cur:
            if sync_all:
                sql = (
                    f"SELECT COUNT(*) FROM `{_mysql_ident(schema)}`.`{_mysql_ident(table)}`"
                )
            else:
                cur.execute(f"USE `{_mysql_ident(database)}`")
                sql = f"SELECT COUNT(*) FROM `{_mysql_ident(table)}`"
            cur.execute(sql)
            row = cur.fetchone()
            return int(row[0]) if row else 0
    finally:
        conn.close()


def _load_profile_ctx(entity_folder: str, config: dict[str, Any]) -> dict[str, Any] | None:
    user = (config.get("user") or "").strip()
    host = (config.get("host") or "").strip()
    if not user or not host:
        return None
    sync_all = bool(config.get("sync_all_databases"))
    database = (config.get("database") or "").strip()
    entity = (config.get("entity") or entity_folder).strip()
    if not sync_all and not database:
        return None
    return {
        "entity": entity,
        "database": database,
        "sync_all": sync_all,
    }


def _iter_cdc_profiles(root: Path) -> list[tuple[str, Path, Path]]:
    out: list[tuple[str, Path, Path]] = []
    profiles = _profiles_root(root)
    if profiles.is_dir():
        for entry in sorted(profiles.iterdir()):
            if not entry.is_dir():
                continue
            cfg = entry / "cdc_config.json"
            if cfg.is_file():
                out.append((entry.name, cfg, entry / "cdc_state.json"))
    # Legacy flat layout
    for entry in sorted(root.iterdir()):
        if not entry.is_dir() or entry.name in ("profiles", "mirror", "vpn") or entry.name.startswith("."):
            continue
        cfg = entry / "cdc_config.json"
        if cfg.is_file() and not any(x[0] == entry.name for x in out):
            out.append((entry.name, cfg, entry / "cdc_state.json"))
    return out


def collect_tables(root: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for entity_folder, config_path, state_path in _iter_cdc_profiles(root):
        try:
            config = json.loads(config_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            print(f"skip {config_path}: {e}", file=sys.stderr)
            continue
        ctx = _load_profile_ctx(entity_folder, config)
        if ctx is None:
            continue
        if not state_path.is_file():
            continue
        try:
            state = json.loads(state_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            print(f"skip {state_path}: {e}", file=sys.stderr)
            continue
        bootstrapped = state.get("bootstrapped_tables") or []
        if not bootstrapped:
            continue

        for qkey in bootstrapped:
            try:
                schema, table_sql = _parse_qkey(ctx["sync_all"], ctx["database"], qkey)
                disk_entity = schema.lower() if ctx["sync_all"] else ctx["entity"]
                entity_dir = _mirror_entity_dir(root, disk_entity)
                mirror_dir = _resolve_mirror_table_dir(entity_dir, table_sql)
                table_path = entity_dir / mirror_dir

                source_count = _mysql_count(
                    config, ctx["sync_all"], ctx["database"], schema, table_sql
                )
                mirror_count = _mirror_live_count(table_path)
                rows.append(
                    {
                        "table": qkey,
                        "source_count": source_count,
                        "mirror_count": mirror_count,
                    }
                )
                print(
                    f"  {qkey}: source={source_count} mirror={mirror_count} "
                    f"diff={source_count - mirror_count}"
                )
            except Exception as e:
                print(f"  skip {qkey}: {e}", file=sys.stderr)

    return rows


def post_consistency_check(
    control_plane_url: str,
    deployment_id: str,
    instance_token: str,
    tables: list[dict[str, Any]],
) -> None:
    payload = {
        "checked_at": datetime.now(timezone.utc).isoformat(),
        "tables": tables,
    }
    if os.environ.get("CONSISTENCY_CHECK_DRY_RUN") == "1":
        print(json.dumps(payload, indent=2))
        return

    url = control_plane_url.rstrip("/") + "/v1/health/consistency-check"
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {instance_token}",
            "X-Bittice-Deployment": deployment_id,
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            print(f"POST {url} → {resp.status}")
            print(resp.read().decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"POST {url} failed ({e.code}): {detail}") from e


def main() -> int:
    dep_id = _env("BITTICE_DEPLOYMENT_ID")
    token = _env("BITTICE_INSTANCE_TOKEN")
    cp_url = _env("BITTICE_CONTROL_PLANE_URL")
    if not dep_id or not token or not cp_url:
        print(
            "Missing BITTICE_DEPLOYMENT_ID, BITTICE_INSTANCE_TOKEN, or "
            "BITTICE_CONTROL_PLANE_URL. Run via run-consistency-check.sh on EC2.",
            file=sys.stderr,
        )
        return 1

    root = _data_root()
    if not root.is_dir():
        print(f"Data root not found: {root}", file=sys.stderr)
        return 1

    print(f"Consistency check: deployment={dep_id} data_root={root}")
    tables = collect_tables(root)
    if not tables:
        print("No bootstrapped tables — nothing to report.")
        return 0

    post_consistency_check(cp_url, dep_id, token, tables)
    return 0


if __name__ == "__main__":
    sys.exit(main())
