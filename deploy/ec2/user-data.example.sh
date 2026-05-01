#!/bin/bash
# Example user-data for Amazon Linux 2023 / Ubuntu on EC2.
# Set BITTICE_IMAGE. Prefer SSM/Secrets Manager for secrets and export to e.g. /etc/bittice.env
set -euxo pipefail

yum install -y docker || (apt-get update && apt-get install -y docker.io)
systemctl enable --now docker

BITTICE_IMAGE="ghcr.io/OWNER/bittice:v0.0.0"

mkdir -p /opt/bittice
cat >/opt/bittice/bittice.env <<EOF
BITTICE_HOST=0.0.0.0
BITTICE_ENGINE_ONLY=1
EOF

# GHCR auth (PAT with read:packages) if the package is private
# echo "$GHCR_TOKEN" | docker login ghcr.io -u USER --password-stdin

docker pull "$BITTICE_IMAGE"

docker run -d --name bittice --restart always \
  -p 3000:3000 -p 8080:8080 -p 50051:50051 \
  --env-file /opt/bittice/bittice.env \
  -v bittice-data:/app/data \
  "$BITTICE_IMAGE"

# Security groups: expose 3000 (saved queries) and 50051 to clients; expose 8080 (admin API) only to trusted ops/VPN.
# For HTTPS, use ALB/nginx in front.
