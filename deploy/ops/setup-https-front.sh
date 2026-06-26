#!/usr/bin/env bash
# Option 1 edge layout: HTTPS REST on :443 (Caddy), admin via SSH tunnel, gRPC public :50051.
#
# Run on EC2 (--local) after Terraform SG allows 443/80 and restricts 8080/50051:
#   sudo ./setup-https-front.sh \
#     --domain dash-sac.prod.parking.net.co \
#     --grpc-domain dash-sac-grpc.prod.parking.net.co
#
# From laptop (updates compose on EC2; run `terraform apply` separately for SG):
#   export AWS_PROFILE=deploy-goparking-prod
#   ./deploy/ops/setup-https-front-cloud.sh \
#     --domain dash-sac.prod.parking.net.co \
#     --grpc-domain dash-sac-grpc.prod.parking.net.co
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
DOMAIN=""
GRPC_DOMAIN=""
DRY_RUN=0
IMAGE="${BITTICE_IMAGE:-ghcr.io/julianrodelo11/bittice:stable}"

usage() {
  cat <<'EOF'
Usage: setup-https-front.sh --domain <rest-host> --grpc-domain <grpc-host> [options]

Options:
  --domain <host>       Public REST hostname (DNS A → this EC2)
  --grpc-domain <host>  Public gRPC hostname (DNS A → this EC2; clients use :50051)
                        Optional if data/.bittice_cloud.json has grpc_domain
  --dry-run             Print compose/Caddyfile only
  -h, --help

After running:
  REST   https://<domain>/
  Admin  ssh -L 8080:127.0.0.1:8080 ubuntu@<ip>  →  http://127.0.0.1:8080
  gRPC   <grpc-domain>:50051 (public)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --domain) DOMAIN="${2:?}"; shift 2 ;;
    --grpc-domain) GRPC_DOMAIN="${2:?}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

if [[ -z "${DOMAIN}" ]]; then
  echo "setup-https-front: --domain is required" >&2
  usage
  exit 1
fi

if [[ -z "${GRPC_DOMAIN}" && -f "${DATA_ROOT}/.bittice_cloud.json" ]]; then
  GRPC_DOMAIN="$(python3 - "${DATA_ROOT}/.bittice_cloud.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1])).get("grpc_domain") or "")
PY
)"
fi

if [[ -z "${GRPC_DOMAIN}" ]]; then
  echo "setup-https-front: --grpc-domain is required (or set grpc_domain in ${DATA_ROOT}/.bittice_cloud.json)" >&2
  usage
  exit 1
fi

for host in "${DOMAIN}" "${GRPC_DOMAIN}"; do
  if [[ "${host}" == *"://"* || "${host}" == *"/"* || "${host}" == *":"* ]]; then
    echo "setup-https-front: use hostname only (no scheme or port): ${host}" >&2
    exit 1
  fi
done

OPT_BITTICE="/opt/bittice"
CADDYFILE="${OPT_BITTICE}/Caddyfile"
COMPOSE="${OPT_BITTICE}/docker-compose.yml"

write_caddyfile() {
  cat >"${CADDYFILE}" <<EOF
${DOMAIN} {
    reverse_proxy bittice:3000
}
EOF
}

write_compose() {
  cat >"${COMPOSE}" <<EOF
services:
  bittice:
    image: "${IMAGE}"
    container_name: bittice
    labels:
      - "com.centurylinklabs.watchtower.enable=true"
    ports:
      - "127.0.0.1:8080:8080"
      - "50051:50051"
    volumes:
      - ${OPT_BITTICE}/data:/app/data
    environment:
      - BITTICE_HOST=0.0.0.0
      - BITTICE_ENGINE_ONLY=1
      - BITTICE_CDC_HEALTH_CHECK_MAX_FAILURES=0
      - BITTICE_CDC_HEALTH_CHECK_INTERVAL_SECS=300
      - BITTICE_CDC_STREAM_SILENCE_TIMEOUT_SECS=90
      - BITTICE_WARM_MAX_TABLE_MB=500
      - BITTICE_WARM_INDICES_ONLY=1
      - BITTICE_QUERY_OPEN_LAZY=1
      - BITTICE_BUFFER_POOL_MB=1024
    restart: unless-stopped
    networks:
      - bittice_net

  caddy:
    image: caddy:2-alpine
    container_name: caddy
    restart: unless-stopped
    ports:
      - "443:443"
      - "80:80"
    volumes:
      - ${CADDYFILE}:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
      - caddy_config:/config
    networks:
      - bittice_net
    depends_on:
      - bittice

  watchtower:
    image: containrrr/watchtower:latest
    container_name: watchtower
    restart: unless-stopped
    environment:
      - DOCKER_API_VERSION=1.44
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    command:
      - --interval
      - "300"
      - --label-enable
      - --cleanup

networks:
  bittice_net:

volumes:
  caddy_data:
  caddy_config:
EOF
}

if [[ "${DRY_RUN}" -eq 1 ]]; then
  write_caddyfile
  write_compose
  echo "--- ${CADDYFILE} ---"; cat "${CADDYFILE}"
  echo "--- ${COMPOSE} ---"; cat "${COMPOSE}"
  echo "gRPC   ${GRPC_DOMAIN}:50051"
  exit 0
fi

log() { echo "[setup-https-front] $*"; }

log "Writing ${CADDYFILE} and ${COMPOSE}…"
write_caddyfile
write_compose

if docker inspect bittice >/dev/null 2>&1; then
  img="$(docker inspect bittice --format '{{.Config.Image}}')"
  [[ -n "${img}" ]] && IMAGE="${img}"
fi

log "Recreating stack (bittice + caddy + watchtower)…"
if systemctl is-active nginx >/dev/null 2>&1; then
  log "Stopping system nginx (conflicts with Caddy on :80)…"
  systemctl stop nginx 2>/dev/null || true
  systemctl disable nginx 2>/dev/null || true
fi

cd "${OPT_BITTICE}"
docker-compose pull bittice 2>/dev/null || true
docker-compose down 2>/dev/null || true
docker rm -f caddy 2>/dev/null || true
docker-compose up -d

log "REST   https://${DOMAIN}/"
log "Admin  ssh -L 8080:127.0.0.1:8080 ubuntu@<this-host>  →  http://127.0.0.1:8080"
log "gRPC   ${GRPC_DOMAIN}:50051 (public — ensure SG allows 50051)"
log "Ensure SG allows 443/80/50051 from internet; 8080 VPC/tunnel only."
