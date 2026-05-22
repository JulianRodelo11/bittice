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
  BITTICE_MYSQL_SSL=0  — disable TLS (not recommended on RDS)
  CONSISTENCY_CHECK_DRY_RUN=1  — print payload, do not POST
  BITTICE_OPS_INCLUDE_AUDIT=1  — also check append-only / audit tables (not recommended)
  BITTICE_OPS_EXTRA_SKIP_TABLES=bittice.foo,bittice.bar  — extra qkeys to skip
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
    from pymysql import err as pymysql_err
except ImportError:
    print("Missing pymysql. Run: pip3 install -r requirements.txt", file=sys.stderr)
    sys.exit(1)

# MySQL: Host 'x' is blocked because of many connection errors
ER_HOST_IS_BLOCKED = 1129

# Append-only / fleet-meta tables: COUNT(*) vs mirror total rows is meaningless
# (each cron INSERT adds RDS rows; CDC copies all of them → false drift).
DEFAULT_SKIP_TABLES: frozenset[str] = frozenset(
    {
        "bittice.consistency_checks",
        "bittice.drift_incidents",
        "bittice.schema_migrations",
    }
)


def _skip_table_qkeys() -> frozenset[str]:
    if _env("BITTICE_OPS_INCLUDE_AUDIT") == "1":
        return frozenset()
    extra = _env("BITTICE_OPS_EXTRA_SKIP_TABLES", "") or ""
    more = {t.strip() for t in extra.split(",") if t.strip()}
    return DEFAULT_SKIP_TABLES | frozenset(more)


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


def _segment_deleted_count(seg: dict[str, Any], seg_dir: Path) -> int:
    """Prefer manifest `deleted_count` (synced by motor v0.1.137+).

    On-disk Roaring bitmaps need the `roaring` Python package; without it a
    size>0 fallback of 1 per segment massively under-counts tombstones and
    inflates mirror live rows on heartbeat-heavy tables.
    """
    dc_manifest = int(seg.get("deleted_count") or 0)
    if not seg_dir.is_dir():
        return dc_manifest
    path = seg_dir / "deleted.bitmap"
    if not path.is_file():
        return dc_manifest
    try:
        import roaring  # type: ignore

        return len(roaring.Bitmap.deserialize(path.read_bytes()))
    except Exception:
        return dc_manifest


def _mirror_live_count(table_dir: Path) -> int:
    """Match engine query semantics: record_count minus tombstones per segment."""
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
        dc = _segment_deleted_count(seg, seg_dir)
        live += max(0, rc - dc)
    return live


def _parse_qkey(sync_all: bool, database: str, qkey: str) -> tuple[str, str]:
    if sync_all:
        if "." not in qkey:
            raise ValueError(f"malformed sync-all qkey: {qkey!r}")
        schema, table = qkey.split(".", 1)
        return schema, table
    return database, qkey


def _mysql_ssl_enabled(host: str) -> bool:
    if _env("BITTICE_MYSQL_SSL", "1") == "0":
        return False
    return "rds.amazonaws.com" in host or host.endswith(".amazonaws.com")


def _mysql_connect(config: dict[str, Any], database: str | None) -> pymysql.connections.Connection:
    """One TCP session per profile per cron run — avoids host_cache blocks from N× connects."""
    host = config["host"]
    port = int(config.get("port") or 3306)
    user = config["user"]
    password = config.get("pass") or config.get("password") or ""
    kwargs: dict[str, Any] = {
        "host": host,
        "port": port,
        "user": user,
        "password": password,
        "database": database,
        "charset": "utf8mb4",
        "connect_timeout": 15,
        "read_timeout": 60,
    }
    if _mysql_ssl_enabled(host):
        kwargs["ssl"] = {"ssl": {}}

    try:
        return pymysql.connect(**kwargs)
    except pymysql_err.OperationalError as e:
        code = e.args[0] if e.args else None
        if code == ER_HOST_IS_BLOCKED:
            print(
                f"MySQL blocked host {host} (error 1129). "
                "Run flush-mysql-host-cache.sh from a machine that can still connect, "
                "or raise RDS max_connect_errors. See deploy/ops/README.md.",
                file=sys.stderr,
            )
            sys.exit(2)
        raise


def _mysql_count_on_conn(
    conn: pymysql.connections.Connection,
    sync_all: bool,
    database: str,
    schema: str,
    table: str,
) -> int:
    with conn.cursor() as cur:
        if sync_all:
            sql = f"SELECT COUNT(*) FROM `{_mysql_ident(schema)}`.`{_mysql_ident(table)}`"
        else:
            cur.execute(f"USE `{_mysql_ident(database)}`")
            sql = f"SELECT COUNT(*) FROM `{_mysql_ident(table)}`"
        cur.execute(sql)
        row = cur.fetchone()
        return int(row[0]) if row else 0


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

        db = None if ctx["sync_all"] else ctx["database"]
        skip = _skip_table_qkeys()
        conn = _mysql_connect(config, db)
        try:
            for qkey in bootstrapped:
                if qkey in skip:
                    print(f"  skip {qkey} (audit/append-only — not compared)")
                    continue
                try:
                    schema, table_sql = _parse_qkey(ctx["sync_all"], ctx["database"], qkey)
                    disk_entity = schema.lower() if ctx["sync_all"] else ctx["entity"]
                    entity_dir = _mirror_entity_dir(root, disk_entity)
                    mirror_dir = _resolve_mirror_table_dir(entity_dir, table_sql)
                    table_path = entity_dir / mirror_dir

                    source_count = _mysql_count_on_conn(
                        conn, ctx["sync_all"], ctx["database"], schema, table_sql
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
                except pymysql_err.OperationalError as e:
                    code = e.args[0] if e.args else None
                    if code == ER_HOST_IS_BLOCKED:
                        raise
                    print(f"  skip {qkey}: {e}", file=sys.stderr)
                except Exception as e:
                    print(f"  skip {qkey}: {e}", file=sys.stderr)
        finally:
            conn.close()

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

    skip = _skip_table_qkeys()
    if skip:
        print(
            f"Consistency check: deployment={dep_id} data_root={root} "
            f"(skipping {len(skip)} audit table(s))"
        )
    else:
        print(f"Consistency check: deployment={dep_id} data_root={root}")
    tables = collect_tables(root)
    if not tables:
        print("No bootstrapped tables — nothing to report.")
        return 0

    post_consistency_check(cp_url, dep_id, token, tables)
    return 0


if __name__ == "__main__":
    sys.exit(main())
