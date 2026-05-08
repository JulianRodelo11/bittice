# Runtime image: packages a pre-built Linux release binary.
# CI: release.yml places bittice-linux-{amd64,arm64} at repo root and builds with BINARY_PATH=...
#
# Manual build (from bittice repo root, binary already copied):
#   cp target/x86_64-unknown-linux-musl/release/bittice bittice-linux-amd64
#   docker build -f deploy/Dockerfile --build-arg BINARY_PATH=bittice-linux-amd64 -t bittice:local .

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libc6 \
    && rm -rf /var/lib/apt/lists/*

ARG BINARY_PATH

WORKDIR /app

COPY ${BINARY_PATH} /usr/local/bin/bittice
RUN chmod +x /usr/local/bin/bittice

COPY deploy/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

EXPOSE 3000 8080 50051

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
