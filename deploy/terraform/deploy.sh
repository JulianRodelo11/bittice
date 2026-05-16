#!/usr/bin/env bash
# Despliega Bittice en EC2 usando la imagen publicada en GHCR.
#
# Uso:
#   ./deploy.sh v0.1.93
#
# Variables de entorno opcionales:
#   BITTICE_SSH_KEY   Ruta a la clave SSH privada (default: ~/.ssh/id_rsa)
#   GHCR_TOKEN        PAT de GitHub con read:packages (solo si la imagen es privada)
#   GHCR_USER         Usuario de GitHub (solo si la imagen es privada)
set -euo pipefail

TAG="${1:?Uso: $0 <tag>  (ej: v0.1.93)}"
SSH_KEY="${BITTICE_SSH_KEY:-$HOME/.ssh/id_rsa}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMAGE="ghcr.io/julianrodelo11/bittice:${TAG}"

log() { echo "[deploy] $*"; }

# Obtener IP de Terraform (debe correrse desde el directorio del script)
cd "$(dirname "${BASH_SOURCE[0]}")"
EC2_IP="$(terraform output -raw public_ip 2>/dev/null)" || {
  echo "Error: no se pudo leer 'public_ip' desde Terraform."
  echo "Asegurate de haber corrido: terraform apply"
  exit 1
}

log "Host: $EC2_IP"
log "Imagen: $IMAGE"

ssh_run() {
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=15 \
      -i "$SSH_KEY" ubuntu@"$EC2_IP" "$@"
}

# 1. Sincronizar data/
if [ -d "$REPO_ROOT/data" ]; then
  log "Sincronizando data/..."
  rsync -avz --progress \
    -e "ssh -o StrictHostKeyChecking=no -i $SSH_KEY" \
    "$REPO_ROOT/data/" ubuntu@"$EC2_IP":/opt/bittice/data/
else
  log "data/ no encontrado, saltando rsync."
fi

# 2. Autenticar en GHCR (solo si la imagen es privada)
if [ -n "${GHCR_TOKEN:-}" ]; then
  log "Autenticando en GHCR..."
  ssh_run "echo '$GHCR_TOKEN' | docker login ghcr.io -u '${GHCR_USER:?GHCR_USER requerido con GHCR_TOKEN}' --password-stdin"
fi

# 3. Descargar imagen
log "Descargando $IMAGE..."
ssh_run "docker pull '$IMAGE'"

# 4. Reemplazar contenedor
log "Reiniciando contenedor..."
ssh_run "docker rm -f bittice 2>/dev/null || true"
ssh_run "docker run -d \
  --name bittice \
  --restart always \
  -p 3000:3000 -p 8080:8080 -p 50051:50051 \
  -e BITTICE_HOST=0.0.0.0 \
  -e BITTICE_ENGINE_ONLY=1 \
  -e BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0 \
  -e BITTICE_CDC_VPN_RESTART_COOLDOWN_SECS=20 \
  -v /opt/bittice/data:/app/data \
  '$IMAGE'"

# 5. Verificar
log "Verificando..."
sleep 5
ssh_run "docker ps --filter name=bittice --format 'table {{.Names}}\t{{.Status}}\t{{.Image}}'"
ssh_run "curl -sf http://localhost:8080 && echo 'OK' || echo 'ADVERTENCIA: health check falló'"

[ -n "${GHCR_TOKEN:-}" ] && ssh_run "docker logout ghcr.io" || true

echo ""
echo "Deploy completado: $IMAGE en $EC2_IP"
