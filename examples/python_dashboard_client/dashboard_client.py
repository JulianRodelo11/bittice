#!/usr/bin/env python3
"""Reference dashboard client for Bittice — "patrón B" (subscribe + debounce + refetch).

Replaces polling loops like
    while True: requests.get("/list-recent-checks"); sleep(5)
with
    1) one REST snapshot on startup,
    2) a gRPC SubscribeUpdates stream that fires whenever any table backing
       the saved op changes,
    3) a 500 ms debounce before a single REST refetch (coalesces bursts of
       events, like 10 CDC rows landing in the same binlog batch).

Cuts engine request volume by ~60x vs. 5-second polling while keeping the
client trivial — no local join/filter/order state to maintain.

Run:
    pip install -r requirements.txt
    BITTICE_API_KEY=<your key> python dashboard_client.py
    # optional overrides:
    #   BITTICE_REST=https://engine.bittice.com
    #   BITTICE_GRPC=engine.bittice.com:50051
    #   BITTICE_QUERY=list-recent-checks
    #   BITTICE_INSECURE_GRPC=1     # plaintext gRPC for local dev
"""
from __future__ import annotations

import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import requests

# ── Generate gRPC stubs on first run so this stays a single-file example.
HERE = Path(__file__).parent
PROTO_DIR = (HERE / "../../proto").resolve()
STUB = HERE / "bittice_pb2.py"
if not STUB.exists():
    subprocess.run(
        [
            sys.executable, "-m", "grpc_tools.protoc",
            f"-I{PROTO_DIR}",
            f"--python_out={HERE}",
            f"--grpc_python_out={HERE}",
            "bittice.proto",
        ],
        check=True,
    )

sys.path.insert(0, str(HERE))
import grpc  # noqa: E402
import bittice_pb2  # noqa: E402
import bittice_pb2_grpc  # noqa: E402

API_KEY = os.environ.get("BITTICE_API_KEY", "")
REST_BASE = os.environ.get("BITTICE_REST", "https://engine.bittice.com").rstrip("/")
GRPC_TARGET = os.environ.get("BITTICE_GRPC", "engine.bittice.com:50051")
QUERY_NAME = os.environ.get("BITTICE_QUERY", "list-recent-checks")
INSECURE = os.environ.get("BITTICE_INSECURE_GRPC") == "1"
DEBOUNCE_SECS = 0.5


def fetch_snapshot() -> dict:
    headers = {"x-api-key": API_KEY} if API_KEY else {}
    r = requests.get(f"{REST_BASE}/{QUERY_NAME}", headers=headers, timeout=10)
    r.raise_for_status()
    return r.json()


def render(snapshot: dict) -> None:
    rows = snapshot.get("data", [])
    ts = time.strftime("%H:%M:%S")
    print(f"[{ts}] {QUERY_NAME}: {len(rows)} row(s)")
    # Replace with your UI write / DB upsert / websocket fanout / etc.


# Single timer guarded by a lock; every new event resets it, so a burst of
# N events within DEBOUNCE_SECS triggers exactly one refetch.
_debounce_timer: threading.Timer | None = None
_debounce_lock = threading.Lock()


def _do_refetch() -> None:
    try:
        render(fetch_snapshot())
    except requests.RequestException as e:
        print(f"refetch error: {e}", file=sys.stderr)


def schedule_refetch() -> None:
    global _debounce_timer
    with _debounce_lock:
        if _debounce_timer is not None:
            _debounce_timer.cancel()
        _debounce_timer = threading.Timer(DEBOUNCE_SECS, _do_refetch)
        _debounce_timer.daemon = True
        _debounce_timer.start()


def open_channel() -> grpc.Channel:
    if INSECURE:
        return grpc.insecure_channel(GRPC_TARGET)
    return grpc.secure_channel(GRPC_TARGET, grpc.ssl_channel_credentials())


def subscribe_forever() -> None:
    metadata = [("x-api-key", API_KEY)] if API_KEY else []
    req = bittice_pb2.SubscribeRequest(query_name=QUERY_NAME)
    while True:
        try:
            channel = open_channel()
            stub = bittice_pb2_grpc.DatabaseStub(channel)
            print(f"subscribed to '{QUERY_NAME}' via {GRPC_TARGET}")
            for _event in stub.SubscribeUpdates(req, metadata=metadata):
                # We don't inspect the event — we only need to know SOMETHING
                # changed. The debounced refetch handles the rest.
                schedule_refetch()
        except grpc.RpcError as e:
            print(f"stream closed ({e.code()}); reconnecting in 2s", file=sys.stderr)
            time.sleep(2)


if __name__ == "__main__":
    render(fetch_snapshot())  # initial snapshot
    subscribe_forever()
