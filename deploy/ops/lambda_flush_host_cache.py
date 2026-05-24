"""Lambda: TRUNCATE performance_schema.host_cache (unblocks EC2 after error 1129)."""
from __future__ import annotations

import os

import pymysql


def _check_auth(event: dict) -> bool:
    secret = (os.environ.get("FLUSH_SECRET") or "").strip()
    if not secret:
        return False
    headers = {k.lower(): v for k, v in (event.get("headers") or {}).items()}
    auth = headers.get("authorization") or ""
    if auth == f"Bearer {secret}":
        return True
    q = event.get("queryStringParameters") or {}
    return (q.get("token") or "") == secret


def handler(event, context):  # noqa: ARG001
    if not _check_auth(event):
        return {"statusCode": 401, "body": "unauthorized"}

    host = os.environ["DB_HOST"]
    port = int(os.environ.get("DB_PORT", "3306"))
    user = os.environ["DB_USER"]
    password = os.environ["DB_PASS"]

    conn = pymysql.connect(
        host=host,
        port=port,
        user=user,
        password=password,
        ssl={"ssl": {}},
        connect_timeout=10,
        read_timeout=30,
    )
    try:
        with conn.cursor() as cur:
            cur.execute("TRUNCATE TABLE performance_schema.host_cache")
        conn.commit()
    finally:
        conn.close()

    return {"statusCode": 200, "body": "host_cache truncated"}
