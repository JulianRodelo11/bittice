# Bittice on the server (no git clone)

> **Working from the Bittice CLI on your laptop?** The usual path is: sync data and save queries locally, then use **Deploy → Build image here + bundle + deploy over SSH** so the image is built on your PC and uploaded to the instance (see `deploy/README.md`, *Full deploy from the Bittice menu*). This document describes the **release zip** workflow, which uses the pre-built **GHCR** image and is geared toward a fresh or separately configured data volume.

This package is built on each GitHub **release** along with the Docker image published to **GHCR**.

## What’s in the package

- `docker-compose.yaml` — production start (ports, data volume).
- `docker-compose.vpn.yaml` — optional, if you use OpenVPN in the container.
- `docker-compose.watchtower.yaml` — optional, [Watchtower](https://containrrr.dev/watchtower/) polls GHCR and recreates the labeled `bittice` service when the image digest changes (see `deploy/actualizacion-automatica-ec2.md` in the repo).
- `.env` — `BITTICE_IMAGE` already points at **this** release’s image.

## Instance requirements

- Linux with **Docker** and **Docker Compose v2**.
- Firewall allows **saved-query REST** (3000), **admin REST** (8080 — create/edit queries, `/_config`; restrict this surface), and **gRPC** (50051) only to who should use each API.

## Steps

1. On the release page, download **`bittice-server-<version>.zip`** (e.g. `bittice-server-v0.1.58.zip`).
2. Copy the zip to the server or download it there with `curl`/`wget` (use the release asset link).
3. Unzip and enter the folder:

   ```bash
   unzip bittice-server-v0.1.58.zip
   cd server-bundle
   ```

4. If the GHCR image is **private**, sign in to the registry (create a token with *read:packages*):

   ```bash
   echo YOUR_TOKEN | docker login ghcr.io -u GITHUB_USER --password-stdin
   ```

5. Start:

   ```bash
   docker compose -f docker-compose.yaml --env-file .env up -d
   ```

6. Verify the container is running:

   ```bash
   docker ps
   docker logs bittice
   ```

## Automatic image updates (optional)

If `BITTICE_IMAGE` uses a registry image (GHCR from this zip is the normal case), you can run **Watchtower** so the host **pulls and recreates** `bittice` when you publish a newer image **for the same tag** (e.g. moving `:latest`) or after you edit `.env` to a new tag and run `compose up` once. Example:

```bash
docker compose -f docker-compose.yaml -f docker-compose.watchtower.yaml --env-file .env up -d
```

The engine can also **log a warning** when GitHub has a newer release than the running binary (`BITTICE_RELEASE_CHECK_INTERVAL_SECS`); that does **not** restart the container. Full detail: **`deploy/actualizacion-automatica-ec2.md`** in the repository.

## Docker only (no zip files)

If you already know the image name for this release:

`ghcr.io/<owner>/<repo>:<tag>`

you can run:

```bash
docker pull ghcr.io/<owner>/<repo>:<tag>
docker run -d --name bittice --restart always \
  -p 3000:3000 -p 8080:8080 -p 50051:50051 \
  -e BITTICE_HOST=0.0.0.0 \
  -e BITTICE_ENGINE_ONLY=1 \
  -v bittice-data:/app/data \
  ghcr.io/<owner>/<repo>:<tag>
```

Data lives in the `bittice-data` volume.

## MySQL / CDC

You do not need the repo to connect Bittice to your database: configuration is in the container’s **data volume**. Ensure the instance has **network** access to MySQL (and a VPN or tunnel if your environment requires it). For the VPN overlay, also use `docker-compose.vpn.yaml` as in `deploy/README.md` in the development repository.

## From a machine where Bittice is already set up (recommended with VPN)

In the development repository you can build a package with `data/`, `.ovpn` profiles, and a `docker-compose` that bind-mounts `./data` and `./vpn` so the server needs no extra manual steps. See *Deploy with a pre-configured local profile* in `deploy/README.md` and the `deploy/scripts/export-server-bundle.sh` script.
