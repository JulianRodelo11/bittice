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

### Verify

```sql
SELECT * FROM consistency_checks
ORDER BY checked_at DESC LIMIT 20;
```

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
