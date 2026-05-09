# Bittice deployment (Docker)

This directory holds the image definition, compose files, and scripts to run Bittice on a server (e.g. EC2) with Docker, without a Rust toolchain on the production host.

## Contents

| File | Purpose |
|------|---------|
| `Dockerfile` | Minimal Debian image that copies a pre-built **Linux** binary (same as the `docker` CI job). |
| `Dockerfile.from-source` | Builds the engine with `cargo` inside the image; useful for local tests without cross-compilation. |
| `docker-compose.production.yaml` | Service with restart policy, ports **3000** (saved queries), **8080** (admin), **50051** (gRPC), named volume for `/app/data`. |
| `docker-compose.bundled.yaml` | Same stack as `production` + VPN with bind mounts for `./data` and `./vpn` (see *Deploy with pre-configured local profile* below). |
| `docker-compose.watchtower.yaml` | Optional overlay: [Watchtower](https://containrrr.dev/watchtower/) polls the registry and recreates labeled containers (use with `BITTICE_IMAGE` from GHCR or another registry). |
| `actualizacion-automatica-ec2.md` | Spanish guide: Watchtower vs log-only release checks; when `docker load` over SSH is not enough for automatic updates. |
| `scripts/build-image-from-source.sh` | `docker build` using `Dockerfile.from-source`. |
| `scripts/build-image-from-binary.sh` | `docker build` like a release, from a Linux `bittice` binary. |
| `scripts/export-server-bundle.sh` | Packs a local project’s `data/` and `.ovpn` files into a folder ready for `docker compose` on the server. |
| `scripts/push-release-tag.sh` | Creates and pushes `v<Cargo.toml version>` so GitHub Actions publishes the GHCR image. |
| `ec2/user-data.example.sh` | Example instance bootstrap (Docker + `docker run`). |
| `.env.example` | Compose variables (copy to `deploy/.env`). |

## Image on GitHub Container Registry

When you push a `v*` (or `beta-v*`) tag, the *Release* workflow builds `linux/amd64` and `linux/arm64` images and publishes a multi-arch manifest at:

`ghcr.io/<owner>/<repository>:<tag>` (same path as on GitHub → *Packages*)

Set your tag and owner in `BITTICE_IMAGE` inside `deploy/.env`.

### Publish a tag (same image Actions uses)

1. Commit your changes (including **`sync_tables` support** if you rely on filtered CDC).
2. Set `version = "…"` in the root **`Cargo.toml`** to match the release you want.
3. From the repo root:

   ```bash
   ./deploy/scripts/push-release-tag.sh
   ```

   That creates and pushes annotated tag **`v` + Cargo version** (e.g. `0.1.66` → `v0.1.66`), which triggers the workflow. Wait for Actions to finish, then `docker pull ghcr.io/<owner>/<repository>:v0.1.66` (copy the URL from GitHub → *Packages*).

Docker **does not** embed MySQL URLs or table lists from the Git build — at runtime it only reads whatever you mount under **`/app/data`** (`profiles/*/cdc_config.json`, including `sync_tables`). The image only supplies the **`bittice`** binary.

## Deploy without cloning the repo (GitHub Actions / release)

Each release attaches **`bittice-server-<tag>.zip`**: it includes `docker-compose.yaml`, `docker-compose.watchtower.yaml`, a `.env` with `BITTICE_IMAGE` pointing to that tag’s image, and **`SERVER_QUICKSTART.md`** (as `README.md` inside the zip). Download the asset from the release page, upload it to the server, and follow the zip’s README. You do not need the source on the instance.

## Run on a server (Compose)

On the instance: install Docker (and *Compose v2*). Copy the `deploy/` directory (or just `docker-compose.production.yaml` and a `.env`) to the server.

```bash
cd deploy
cp .env.example .env
# edit .env: BITTICE_IMAGE, ports, BITTICE_HOST, etc.
docker compose -f docker-compose.production.yaml --env-file .env pull
docker compose -f docker-compose.production.yaml --env-file .env up -d
```

Compose reads `BITTICE_IMAGE` and other `BITTICE_*` from the file; if you do not use `.env`, set at least `BITTICE_IMAGE` in the environment or pass another `--env-file`.

Data is stored in the Docker volume `bittice-data` (it survives container recreation unless you remove the volume).

**Automatic image updates:** the Bittice container cannot replace its own image. For EC2 to pull a newer image from a registry without manual steps, run Watchtower alongside compose (labeled service). See [`actualizacion-automatica-ec2.md`](actualizacion-automatica-ec2.md) and:

```bash
cd deploy
docker compose -f docker-compose.production.yaml -f docker-compose.watchtower.yaml --env-file .env up -d
```

**VPN:** run your VPN client on the host (or network layer) before starting the container. Bittice Docker images no longer embed OpenVPN/tun management.

## Deploy with a pre-configured local profile (VPN + CDC)

If you already connected and synced on your machine (UI or CLI) and you do not want to manually copy `vpn` or reconfigure entities on the server:

1. From the **project root** (where `data/` lives), set the image you will use in production (e.g. a GHCR tag):
   ```bash
   export BITTICE_IMAGE=ghcr.io/<owner>/<repository>:<tag>
   ./deploy/scripts/export-server-bundle.sh my-server-bundle
   ```
2. Upload the `my-server-bundle` folder (or a zip of it) to the Linux host. The bundle includes:
   - `data/` (entities, `cdc_config.json` with the VPN profile, tables, etc.),
   - `vpn/` with the referenced `.ovpn` files,
   - `docker-compose.yaml` (from `docker-compose.bundled.yaml`) and a `.env` with `BITTICE_IMAGE` and the OpenVPN environment.
3. On the server, if the image is private: `docker login` to the registry, then:
   ```bash
   cd my-server-bundle
   docker compose --env-file .env pull
   docker compose --env-file .env up -d
   ```

`BITTICE_PROJECT_ROOT` lets you run the script from another directory while pointing at the project. The container uses the same paths the engine resolves (including `data/vpn/...` and `/app/vpn/...` via `resolve_ovpn_path`). Treat the bundle as a secret: it contains database credentials and VPN material.

## Full deploy from the Bittice menu (Docker + SSH)

With the interactive app (`bittice` with no extra args), open **Deploy → Build image + bundle + deploy over SSH (full)**. That flow (see `src/repl/deploy_pipeline.rs`) will:

1. Build the runtime image with `deploy/Dockerfile.from-source` (optional `docker buildx` for `linux/amd64` or `linux/arm64` to match the server).
2. Run `deploy/scripts/export-server-bundle.sh` into a staging directory (your `data/`, `vpn/`, compose, `.env` with the same image tag).
3. `docker save | ssh … docker load` so the instance has the image without a public registry.
4. Stream the bundle with `tar` over SSH (preserves `.env` and other dotfiles).
5. `ssh` into the server and run `docker compose pull && docker compose up -d` in `~/<folder>`.

You need Docker running locally, `ssh` with key-based auth to the server, and on the server: Docker with Compose v2 and `docker` usable by your SSH user. No separate shell steps are required unless something fails (check the error message).

## Build the image locally

**From source (Rust inside Docker):**

```bash
./deploy/scripts/build-image-from-source.sh
```

**From a built Linux binary** (same as CI):

```bash
./deploy/scripts/build-image-from-binary.sh path/to/bittice
```

**Push to Amazon ECR** (after `aws ecr get-login-password` or equivalent):

```bash
docker tag bittice:local 123456789012.dkr.ecr.us-east-1.amazonaws.com/my-bittice:v0.1.0
docker push 123456789012.dkr.ecr.us-east-1.amazonaws.com/my-bittice:v0.1.0
```

On EC2, set `BITTICE_IMAGE` to the ECR URI and grant the instance/role `ecr:BatchGetImage` and `ecr:GetDownloadUrlForLayer` if you use least-privilege policies.

## Common environment variables

The application reads `BITTICE_HOST` (e.g. `0.0.0.0`), `BITTICE_ENGINE_ONLY` (blocks interactive/`cdc`/`setup` CLI inside the container — official compose sets `1`), `BITTICE_DISABLE_CDC_AUTOSTART`, `BITTICE_ENTITY`, VPN directory variables, and others (see the code). MySQL credentials do not belong in the image; they live under the data volume `/app/data` per the CDC configuration flow.

## License

Use of Bittice on your own infrastructure is under *Elastic License 2.0* (see the repository). You may not offer Bittice as a hosted service to others except as allowed by the license.
