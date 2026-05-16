#!/usr/bin/env bash
# Despliega Bittice en la EC2 simulada con Floci.
#
# Uso:
#   export BITTICE_IMAGE=ghcr.io/julianrodelo11/bittice:v0.1.93
#   INSTANCE_ID=i-xxx SSH_PORT=2200 ./deploy/floci/ec2-deploy.sh
#
# Asume que ec2-start.sh ya corrió y la instancia tiene Docker listo.
set -euo pipefail

BITTICE_IMAGE="${BITTICE_IMAGE:?Exporta BITTICE_IMAGE}"
SSH_PORT="${SSH_PORT:?Exporta SSH_PORT (ej: 2200)}"
SSH_KEY="${FLOCI_SSH_KEY:-$HOME/.ssh/id_rsa}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REMOTE_DIR="/opt/bittice"

log() { echo "[ec2-deploy] $*"; }

run() {
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
      -i "$SSH_KEY" -p "$SSH_PORT" root@localhost "$@"
}

push() {
  rsync -az --progress \
    -e "ssh -o StrictHostKeyChecking=no -i $SSH_KEY -p $SSH_PORT" \
    "$@"
}

# 1. Copiar data/ a la instancia
log "Sincronizando data/..."
run "mkdir -p $REMOTE_DIR/data"
push "$REPO_ROOT/data/" root@localhost:"$REMOTE_DIR/data/"

# 2. Crear .env mínimo en la instancia
log "Configurando .env..."
run "cat > $REMOTE_DIR/.env" <<EOF
BITTICE_IMAGE=$BITTICE_IMAGE
BITTICE_HOST=0.0.0.0
BITTICE_ENGINE_ONLY=1
BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
BITTICE_CDC_VPN_RESTART_COOLDOWN_SECS=20
EOF

# 3. Pull + run directo (sin docker compose)
log "Descargando imagen..."
run "docker pull $BITTICE_IMAGE"

log "Levantando Bittice..."
run "docker rm -f bittice 2>/dev/null || true"
run "docker run -d \
  --name bittice \
  --restart always \
  -p 3000:3000 -p 8080:8080 -p 50051:50051 \
  --env-file $REMOTE_DIR/.env \
  -v $REMOTE_DIR/data:/app/data \
  $BITTICE_IMAGE"

# 5. Verificar
log "Verificando..."
for _ in $(seq 1 15); do
  run "curl -sf http://localhost:8080" &>/dev/null && break || sleep 3
done
run "docker ps --filter name=bittice --format 'Status: {{.Status}}'"

echo ""
echo "Bittice corriendo en la EC2 simulada."
echo ""
echo "Port-forward local:"
echo "  ssh -L 3000:localhost:3000 -L 8080:localhost:8080 -L 50051:localhost:50051 \\"
echo "      -i $SSH_KEY -p $SSH_PORT root@localhost -N"
