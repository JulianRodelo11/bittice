# Bittice dashboard client — patrón B

Reference client showing how to consume a Bittice saved op as a live view
without polling. Adapt the same pattern in any language with a gRPC client
(Node, Go, Rust, Java…) — the engine side needs no changes.

## What it does

1. **Snapshot once** via `GET /<saved_op>` (REST).
2. **Subscribe** to `Database/SubscribeUpdates` (gRPC streaming). The engine
   emits a `UpdateEvent` for every CDC row that lands in one of the tables
   the saved op references — JOINed tables included.
3. **Debounce** events for 500 ms so a burst of CDC rows in the same binlog
   batch coalesces into one refetch.
4. **Refetch** the saved op via REST and rerender.

Result: the client is always within ~500 ms of source-of-truth, with one
request per actual change instead of one request per poll interval.

## Why not apply deltas client-side?

Saved ops have JOINs, filters, ORDER BY, projections. Reapplying a raw
`UpdateEvent` against a stored view is a lot of client code. Letting the
engine recompute the full view on each notification is trivial and
correct, and saved ops are designed to be cheap to re-run.

## Run

```bash
pip install -r requirements.txt
BITTICE_API_KEY=<your-key> python dashboard_client.py
```

Output:

```
[19:42:11] list-recent-checks: 100 row(s)
subscribed to 'list-recent-checks' via engine.bittice.com:50051
[19:42:18] list-recent-checks: 100 row(s)   # one CDC batch triggered one refetch
[19:47:01] list-recent-checks: 100 row(s)
```

## Configuration

| env var               | default                              | purpose                                |
|-----------------------|--------------------------------------|----------------------------------------|
| `BITTICE_API_KEY`     | _none_                               | sent as `x-api-key` on REST + gRPC     |
| `BITTICE_REST`        | `https://engine.bittice.com`         | REST base for snapshot + refetch       |
| `BITTICE_GRPC`        | `engine.bittice.com:50051`           | gRPC target for subscription           |
| `BITTICE_QUERY`       | `list-recent-checks`                 | saved op name                          |
| `BITTICE_INSECURE_GRPC` | _unset_                            | set to `1` for plaintext (local dev)   |

## Adapting to other languages

Generate stubs from `proto/bittice.proto` with your toolchain's protoc
plugin, then translate the three pieces:

- `fetch_snapshot()` — any HTTP client
- `subscribe_forever()` — async iteration over `SubscribeUpdates` with
  reconnect on `RpcError`
- `schedule_refetch()` — any debouncer (lodash `debounce`, Tokio
  `time::sleep_until`, etc.)
