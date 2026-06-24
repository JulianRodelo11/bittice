#!/usr/bin/env bash
# Option 1 edge layout: HTTPS REST on :443 (Caddy), admin via SSH tunnel, gRPC VPC-only.
#
# Run on EC2 (--local) after Terraform SG allows 443/80 and restricts 8080/50051:
#   sudo ./setup-https-front.sh --domain dash-sac.dev.parking.net.co
#
# From laptop (updates compose on EC2; run `terraform apply` separately for SG):
#   export AWS_PROFILE=deploy-goparking
#   ./deploy/ops/setup-https-front-cloud.sh --domain dash-sac.dev.parking.net.co
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_ROOT="${BITTICE_DATA_ROOT:-/opt/bittice/data}"
DOMAIN=""
DRY_RUN=0
IMAGE="${BITTICE_IMAGE:-ghcr.io/julianrodelo11/bittice:stable}"

usage() {
  cat <<'EOF'
Usage: setup-https-front.sh --domain <hostname> [options]

Options:
  --domain <host>     Public REST hostname (DNS A → this EC2), e.g. dash-sac.dev.parking.net.co
  --dry-run           Print compose/Caddyfile only
  -h, --help

After running:
  REST   https://<domain>/
  Admin  ssh -L 8080:127.0.0.1:8080 ubuntu@<ip>  →  http://127.0.0.1:8080
  gRPC   <private-ip>:50051 from VPC, or SSH -L 50051:127.0.0.1:50051
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --domain) DOMAIN="${2:?}"; shift 2 ;;
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

if [[ "${DOMAIN}" == *"://"* || "${DOMAIN}" == *"/"* || "${DOMAIN}" == *":"* ]]; then
  echo "setup-https-front: use hostname only (no scheme or port)" >&2
  exit 1
fi

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
cd "${OPT_BITTICE}"
docker-compose pull bittice 2>/dev/null || true
docker-compose down 2>/dev/null || docker compose down 2>/dev/null || true
docker rm -f caddy 2>/dev/null || true
(docker-compose up -d 2>/dev/null || docker compose up -d)

log "REST   https://${DOMAIN}/"
log "Admin  ssh -L 8080:127.0.0.1:8080 ubuntu@<this-host>  →  http://127.0.0.1:8080"
log "gRPC   :50051 from VPC only (security group must restrict to VPC CIDR)"
log "Ensure SG allows 443/80 from internet and blocks public 3000/8080/50051."
