#!/usr/bin/env bash
# Lanza una EC2 simulada con Floci y aplica los límites reales del tipo de instancia.
#
# Uso:
#   INSTANCE_TYPE=t3.medium ./deploy/floci/ec2-start.sh
#
# Tipos soportados: t3.micro t3.small t3.medium t3.large t3.xlarge t3.2xlarge
#
# Requisitos:
#   - Floci corriendo: docker compose -f deploy/floci/docker-compose.floci.yaml up -d
#   - AWS CLI instalado
#   - Par de claves SSH en ~/.ssh/id_rsa (o FLOCI_SSH_KEY)
set -euo pipefail

export AWS_ENDPOINT_URL="${FLOCI_ENDPOINT:-http://localhost:4566}"
export AWS_DEFAULT_REGION="${FLOCI_REGION:-us-east-1}"
export AWS_ACCESS_KEY_ID=test
export AWS_SECRET_ACCESS_KEY=test

SSH_KEY="${FLOCI_SSH_KEY:-$HOME/.ssh/id_rsa}"
INSTANCE_TYPE="${INSTANCE_TYPE:-t3.medium}"
KEY_NAME="bittice-key"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { echo "[ec2-start] $*"; }

# Specs reales de cada tipo (CPUs, RAM en MB)
# Ref: https://aws.amazon.com/ec2/instance-types/t3/
case "$INSTANCE_TYPE" in
  t3.micro)   VCPUS=2; MEMORY_MB=1024  ;;
  t3.small)   VCPUS=2; MEMORY_MB=2048  ;;
  t3.medium)  VCPUS=2; MEMORY_MB=4096  ;;
  t3.large)   VCPUS=2; MEMORY_MB=8192  ;;
  t3.xlarge)  VCPUS=4; MEMORY_MB=16384 ;;
  t3.2xlarge) VCPUS=8; MEMORY_MB=32768 ;;
  *) log "ERROR: tipo '$INSTANCE_TYPE' no reconocido. Usa: t3.micro t3.small t3.medium t3.large t3.xlarge t3.2xlarge"; exit 1 ;;
esac
log "Tipo: $INSTANCE_TYPE → ${VCPUS} vCPU, ${MEMORY_MB} MB RAM"

# 1. Registrar clave pública
log "Registrando clave SSH..."
aws ec2 delete-key-pair --key-name "$KEY_NAME" 2>/dev/null || true
aws ec2 import-key-pair \
  --key-name "$KEY_NAME" \
  --public-key-material "$(base64 < "${SSH_KEY}.pub")"

# 2. Lanzar instancia
log "Lanzando instancia (ami-ubuntu2204, $INSTANCE_TYPE)..."
INSTANCE_ID=$(aws ec2 run-instances \
  --image-id ami-ubuntu2204 \
  --instance-type "$INSTANCE_TYPE" \
  --key-name "$KEY_NAME" \
  --user-data "file://$SCRIPT_DIR/user-data.sh" \
  --query 'Instances[0].InstanceId' \
  --output text)
log "Instancia: $INSTANCE_ID"

# 3. Esperar estado running
log "Esperando estado running..."
for _ in $(seq 1 30); do
  STATE=$(aws ec2 describe-instances \
    --instance-ids "$INSTANCE_ID" \
    --query 'Reservations[0].Instances[0].State.Name' \
    --output text)
  [ "$STATE" = "running" ] && break
  sleep 2
done
log "Estado: $STATE"

# 4. Aplicar límites de CPU y RAM al contenedor que Floci creó
# Floci lanza un contenedor Docker real; docker update aplica los límites del tipo de instancia.
log "Aplicando límites de recursos ($VCPUS vCPU, ${MEMORY_MB}m RAM)..."
CONTAINER_ID=$(docker ps --filter "name=floci-ec2-${INSTANCE_ID}" --format "{{.ID}}")
if [ -n "$CONTAINER_ID" ]; then
  docker update \
    --cpus "$VCPUS" \
    --memory "${MEMORY_MB}m" \
    --memory-swap "${MEMORY_MB}m" \
    "$CONTAINER_ID"
  log "Límites aplicados al contenedor $CONTAINER_ID"
else
  log "WARN: no se encontró el contenedor; límites no aplicados"
fi

# 5. Recrear el contenedor con el socket de Docker del host montado
# Floci lanza el contenedor sin acceso a Docker; lo recreamos preservando la clave SSH inyectada.
CONTAINER_NAME="floci-ec2-${INSTANCE_ID}"
log "Montando socket Docker del host en la instancia..."

# Obtener el puerto SSH que Floci asignó
HOST_SSH_PORT=$(docker inspect "$CONTAINER_NAME" \
  --format '{{range $p, $c := .NetworkSettings.Ports}}{{if eq $p "22/tcp"}}{{(index $c 0).HostPort}}{{end}}{{end}}')

# Commit preserva la clave pública que Floci inyectó en /root/.ssh/authorized_keys
docker commit "$CONTAINER_NAME" bittice-ec2-img
docker stop "$CONTAINER_NAME"
docker rm "$CONTAINER_NAME"
docker run -d \
  --name "$CONTAINER_NAME" \
  -p "${HOST_SSH_PORT}:22" \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --cpus "$VCPUS" \
  --memory "${MEMORY_MB}m" \
  --memory-swap "${MEMORY_MB}m" \
  bittice-ec2-img \
  tail -f /dev/null
docker image rm bittice-ec2-img 2>/dev/null || true
log "Contenedor recreado con socket Docker."

# 6. Ejecutar user-data via docker exec
log "Ejecutando user-data (instala SSH y docker CLI)..."
docker exec "$CONTAINER_NAME" bash /tmp/user-data.sh
log "User-data completado."

# 7. Detectar puerto SSH
log "Buscando puerto SSH..."
SSH_PORT=""
for PORT in $(seq 2200 2299); do
  if ssh -o StrictHostKeyChecking=no -o ConnectTimeout=2 -o BatchMode=yes \
         -i "$SSH_KEY" -p "$PORT" root@localhost "echo ok" &>/dev/null; then
    SSH_PORT=$PORT
    break
  fi
done

[ -z "$SSH_PORT" ] && { log "ERROR: SSH no disponible en 2200-2299"; exit 1; }
log "SSH listo en puerto $SSH_PORT"
log "Instancia lista."

echo ""
echo "INSTANCE_ID=$INSTANCE_ID"
echo "SSH_PORT=$SSH_PORT"
echo ""
echo "Conectar:  ssh -i $SSH_KEY -p $SSH_PORT root@localhost"
echo "Siguiente: INSTANCE_ID=$INSTANCE_ID SSH_PORT=$SSH_PORT ./deploy/floci/ec2-deploy.sh"
