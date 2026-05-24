# Fleet ops scripts (Bittice cloud only)

Scripts in this directory are **not** part of the open-source motor that customers install. They run on **your** managed EC2 instances (or a bastion with access to `/opt/bittice/data`) to feed the control plane and CloudWatch alarms.

## Consistency check reporter

Compares `COUNT(*)` on the customer MySQL source vs live row counts in the local mirror (from `manifest.json`), then `POST`s to:

`{BITTICE_CONTROL_PLANE_URL}/v1/health/consistency-check`

Same auth as heartbeat: `Authorization: Bearer tok_…` + `X-Bittice-Deployment: dep_…`.

### Install on EC2

```bash
# From your laptop (after terraform apply)
cd deploy/terraform
EC2_IP="$(terraform output -raw public_ip)"
ssh -i ~/.ssh/id_rsa ubuntu@"$EC2_IP" 'sudo mkdir -p /opt/bittice/ops'
rsync -avz -e "ssh -i ~/.ssh/id_rsa" ../ops/ ubuntu@"$EC2_IP":/opt/bittice/ops/
ssh -i ~/.ssh/id_rsa ubuntu@"$EC2_IP" 'chmod +x /opt/bittice/ops/*.sh'

# One-shot test (dry run — no POST)
ssh -i ~/.ssh/id_rsa ubuntu@"$EC2_IP" \
  'CONSISTENCY_CHECK_DRY_RUN=1 /opt/bittice/ops/run-consistency-check.sh'

# Cron every 5 minutes (ubuntu user)
ssh -i ~/.ssh/id_rsa ubuntu@"$EC2_IP" \
  '(crontab -l 2>/dev/null | grep -v consistency-check; echo "*/5 * * * * /opt/bittice/ops/run-consistency-check.sh >> /var/log/bittice-consistency.log 2>&1") | crontab -'
```

### Requirements on the instance

- Docker container `bittice` running with `BITTICE_DEPLOYMENT_ID`, `BITTICE_INSTANCE_TOKEN`, `BITTICE_CONTROL_PLANE_URL` (wizard / cloud deploy compose).
- Host `python3` + `pymysql` (installed automatically by `run-consistency-check.sh`).
- EC2 can reach the customer MySQL host in `cdc_config.json` (same VPC/security groups as CDC).
- Data at `/opt/bittice/data` (default), with `profiles/*/cdc_state.json` listing `bootstrapped_tables`.

### Tablas que se comparan

Por defecto el cron **solo** reporta tablas de negocio/catálogo. **No** compara:

- `bittice.consistency_checks` — historial de cada reporte (append-only)
- `bittice.drift_incidents` — historial de incidentes
- `bittice.schema_migrations` — migraciones aplicadas

Compararlas con `COUNT(*)` siempre generaba drift falso. Para incluirlas: `BITTICE_OPS_INCLUDE_AUDIT=1`.

El conteo en mirror usa `deleted_count` del `manifest.json` (motor v0.1.137+). No hace falta `pip install roaring` en EC2; si lo instalas, el reporter puede validar contra el bitmap en disco.

### Verify

```sql
SELECT * FROM consistency_checks
ORDER BY checked_at DESC LIMIT 20;
```

Si quedaron incidentes abiertos en tablas de auditoría por un run anterior:

```sql
UPDATE drift_incidents SET status='resolved', resolved_at=NOW(3)
WHERE table_name IN ('bittice.consistency_checks','bittice.drift_incidents')
  AND status='open';
```

Si el API devuelve 500 al cerrar un incidente con drift=0, aplica la migración
`bittice-db/migrations/0018_drift_incidents_open_only_unique.sql` (índice único
solo para filas `open`).

### Evitar bloqueo RDS `Host is blocked` (error 1129)

MySQL cuenta **cada intento de conexión TCP fallido** hacia la IP de la EC2. El script viejo abría **una conexión por tabla** (~11 cada 5 min) y sin TLS; eso llenaba el `host_cache` y RDS bloqueaba `172.31.x.x`.

**Qué hicimos en el script (v2):**

| Cambio | Por qué |
|--------|---------|
| **1 conexión MySQL por perfil** por ejecución del cron | Menos handshakes |
| **TLS** hacia `*.rds.amazonaws.com` | Evita fallos SSL que cuentan como error |
| **Salida inmediata** si ya hay 1129 | No dispara 11 intentos seguidos |

### Auto-recuperación (sin revisar a mano)

Dos capas — configúralas **una vez**:

**1. Prevención (recomendado primero)** — sube el umbral de errores en RDS:

```bash
AWS_PROFILE=bittice ./deploy/ops/ensure-rds-max-connect-errors.sh
```

**2. Lambda de rescate** — si aun así aparece el 1129, el cron en EC2 llama a una Lambda en la VPC que hace el `TRUNCATE host_cache` (IP distinta a la EC2, no bloqueada):

```bash
# En tu Mac, una vez:
AWS_PROFILE=bittice ./deploy/ops/setup-flush-lambda.sh
# Genera deploy/ops/flush-lambda.env → cópialo a la EC2:
scp deploy/ops/flush-lambda.env ubuntu@<EC2>:/opt/bittice/ops/
```

`run-consistency-check.sh` carga ese archivo; el reporter, al ver error 1129, invoca la URL, espera 2 s y **reintenta** MySQL.

**Rescate manual** (solo si Lambda no está configurada):

```bash
./deploy/ops/flush-mysql-host-cache.sh
```

**Prevención en RDS (alternativa manual en consola):** en el parameter group de la instancia MySQL, subir:

```text
max_connect_errors = 1000000
```

(AWS Console → RDS → Parameter groups → Modify → Apply pending reboot si hace falta.)

**Log del cron:** por defecto `/var/log/bittice-consistency.log`; si no hay permiso de escritura, `install-cron.sh` usa `/opt/bittice/ops/consistency.log`.

CloudWatch alarm `bittice-deployment-stale` uses metric `StaleDeploymentCount` (published by your separate metric script from the `stale-deployments` saved op on `engine.bittice.com`).

### Files

| File | Role |
|------|------|
| `consistency_check_reporter.py` | Count + POST |
| `run-consistency-check.sh` | Reads docker env, runs Python on host |
| `requirements.txt` | `pymysql` only (stdlib `urllib` for HTTP) |

Do **not** bundle these into the public `ghcr.io/.../bittice` image or customer release zips.

### Mirror repair (segment bloat)

Heartbeat `UPDATE`s tombstone+append one row per PK but can leave hundreds of micro-segments on disk. Queries still return the correct row count; only disk and naive `record_count` sums look inflated.

After the motor image rolls out on EC2 (see below), optionally compact bloated tables:

```bash
chmod +x /opt/bittice/ops/repair-mirror.sh
sudo /opt/bittice/ops/repair-mirror.sh bittice deployments usage_hours api_keys
```

(`repair-mirror.sh` defaults to `BITTICE_IMAGE=ghcr.io/julianrodelo11/bittice:stable`.)

### Motor rollout (no manual upload to EC2)

1. Commit the motor fixes and push a **version tag** (e.g. `v0.1.136`) — **not** a beta/alpha tag if you want `:stable` promotion.
2. GitHub Actions `release.yml` builds, publishes GHCR, and points **`ghcr.io/.../bittice:stable`** at that build.
3. EC2 with **Watchtower** (see `deploy/docker-compose.watchtower.yaml`) recreates the `bittice` container within ~5 minutes. CDC resumes from saved binlog; env/volumes are preserved.

Do **not** `scp` binaries or run `deploy/terraform/deploy.sh` only to get code changes — that script is for initial/rsync `data/`, not the normal release path.

Sync **only** `deploy/ops/` to the host if the Python reporter changed before the next full deploy:

```bash
rsync -avz deploy/ops/ ubuntu@<EC2>:/opt/bittice/ops/
```
