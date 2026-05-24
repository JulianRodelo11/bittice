# Fleet ops scripts (Bittice cloud only)

Scripts in this directory run on **your** managed EC2 instances (or a bastion) to keep RDS healthy. They are **not** part of the open-source motor that customers install.

Consistency checks and drift diagnostics no longer live here — the motor
itself ([`src/server/self_health.rs`](../../src/server/self_health.rs))
self-vigila desde la v0.1.138. Cadence, on/off, watch lists, auto-repair —
todo configurable desde `bittice.engine_configs`. Cliente no instala nada
más.

## What's left

| File | Purpose |
|------|---------|
| `repair-mirror.sh` | Manual segment compaction — escape hatch when a table has bloat that auto-repair can't resolve. |
| `flush-mysql-host-cache.sh` | Manual `TRUNCATE performance_schema.host_cache` on the source RDS. Use when error 1129 ("Host is blocked") shows up in the engine logs. |
| `ensure-rds-max-connect-errors.sh` | One-time setter for `max_connect_errors=1000000` on the source RDS parameter group. Prevention against 1129. |
| `setup-flush-lambda.sh` + `lambda_flush_host_cache.py` | Lambda-in-VPC that the motor can invoke to flush `host_cache` (no longer needed by the motor itself, kept as manual recovery). |

## Drift visibility

```sql
-- ¿Está el motor reportando? (no debería pasar > interval secs sin un check)
SELECT MAX(checked_at) FROM consistency_checks WHERE deployment_id = ?;

-- Drift activo
SELECT * FROM drift_incidents WHERE status = 'open' AND deployment_id = ?;

-- Por qué pasó (root cause snapshot)
SELECT * FROM drift_diagnostics
  WHERE deployment_id = ? ORDER BY captured_at DESC LIMIT 20;
```

## Operations metering (billing)

Desde v0.1.141 el motor cuenta cada operación facturable y la reporta vía
heartbeat extra blob → tabla `request_buckets` (deployment_id, hour_bucket,
op_type, request_count).

Qué cuenta como operation:

| Acción | ¿Cuenta? |
|---|---|
| GET REST a saved op con respuesta 2xx | 1 unary |
| gRPC unary (search, execute_*_unary) | 1 unary |
| Cada notificación entregada en `SubscribeUpdates` | 1 notification |
| Abrir un SubscribeUpdates stream | 0 (solo cuentan las notifs) |
| Admin endpoints (`/_config`, `/_entities`) | 0 |
| Respuestas 4xx / 5xx | 0 |

```sql
-- Total mensual por usuario (suma todos sus deployments + op_types)
SELECT u.email, u.plan, SUM(rb.request_count) AS month_total
FROM users u
JOIN deployments d ON d.user_id = u.id
JOIN request_buckets rb ON rb.deployment_id = d.id
WHERE rb.hour_bucket >= DATE_FORMAT(NOW(), '%Y-%m-01')
GROUP BY u.id;

-- Por op_type para análisis de margen interno
SELECT rb.op_type, SUM(rb.request_count) AS total
FROM request_buckets rb
WHERE rb.deployment_id = ? AND rb.hour_bucket >= NOW() - INTERVAL 7 DAY
GROUP BY rb.op_type;
```

El motor también expone el total al cliente vía saved op
`user-operations` (auth por `bittice_api_key`, lookup por hash de token):

```bash
curl -H "Authorization: Bearer bk_live_..." \
     http://<motor>:3000/user-operations
```

### Trampa de diseño documentada

**Toda tabla nueva en el control plane debe tener un PK single-column
(idealmente `id BIGINT UNSIGNED AUTO_INCREMENT`).** La composite PK va como
`UNIQUE KEY` aparte.

Razón: el motor mirrorea por CDC y al bootstrap escoge la PRIMERA columna del
PK como partition key del mirror. Si el PK es compuesto y varias filas comparten
ese primer valor (típico: `deployment_id` como primera columna), colisionan en
el mismo slot del mirror y solo una sobrevive. El motor reporta drift
permanente y nunca resuelve. La migración 0022 arregló `request_buckets`
después de caer en esta trampa — usa ese patrón.

## Configuración por deployment

Defaults globales en `engine_config_defaults` (1 fila), override por deployment en `engine_configs`. El motor pollea `/v1/config` cada 60s, así que cambios reflejan en menos de 1 min sin redeploy ni reinicio del contenedor.

```sql
-- Apagar self_health para un cliente
INSERT INTO engine_configs (deployment_id, self_health_enabled) VALUES (?, 0)
  ON DUPLICATE KEY UPDATE self_health_enabled = 0;

-- Bajar la cadencia (RDS saturada)
INSERT INTO engine_configs (deployment_id, self_health_interval_secs) VALUES (?, 900)
  ON DUPLICATE KEY UPDATE self_health_interval_secs = 900;

-- Excluir una tabla específica del watch
INSERT INTO engine_configs (deployment_id, watch_denylist) VALUES (?, '["myapp.audit_log"]')
  ON DUPLICATE KEY UPDATE watch_denylist = '["myapp.audit_log"]';
```
