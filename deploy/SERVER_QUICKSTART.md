# Bittice en el servidor (sin clonar el repositorio)

Este paquete se genera en cada **release** de GitHub junto con la imagen Docker publicada en **GHCR**.

## Qué incluye

- `docker-compose.yaml` — arranque en producción (puertos, volumen de datos).
- `docker-compose.vpn.yaml` — opcional, si usas OpenVPN en contenedor.
- `.env` — ya trae `BITTICE_IMAGE` apuntando a la imagen de **este** release.

## Requisitos en la instancia

- Linux con **Docker** y **Docker Compose v2**.
- Puertos abiertos en el firewall solo hacia quien deba usar la API (3000 REST, 50051 gRPC).

## Pasos

1. En la página del release en GitHub, descarga **`bittice-server-<versión>.zip`** (por ejemplo `bittice-server-v0.1.58.zip`).
2. Sube el zip al servidor o descárgalo allí con `curl`/`wget` (enlace del asset del release).
3. Descomprime y entra en la carpeta:

   ```bash
   unzip bittice-server-v0.1.58.zip
   cd server-bundle
   ```

4. Si la imagen en GHCR es **privada**, inicia sesión en el registro (crea un token con permiso *read:packages*):

   ```bash
   echo TU_TOKEN | docker login ghcr.io -u TU_USUARIO_GITHUB --password-stdin
   ```

5. Arranca:

   ```bash
   docker compose -f docker-compose.yaml --env-file .env up -d
   ```

6. Comprueba que el contenedor está en marcha:

   ```bash
   docker ps
   docker logs bittice
   ```

## Solo con Docker (sin archivos del zip)

Si ya conoces el nombre de la imagen de este release:

`ghcr.io/<propietario>/<repo>:<tag>`

puedes hacer:

```bash
docker pull ghcr.io/<propietario>/<repo>:<tag>
docker run -d --name bittice --restart always \
  -p 3000:3000 -p 50051:50051 \
  -e BITTICE_HOST=0.0.0.0 \
  -v bittice-data:/app/data \
  ghcr.io/<propietario>/<repo>:<tag>
```

Los datos quedan en el volumen `bittice-data`.

## MySQL / CDC

Conectar Bittice a tu base no depende de clonar el repo: la configuración vive en el **volumen de datos** del contenedor. Asegúrate de que la instancia tenga **conectividad de red** hasta el MySQL (y VPN si tu entorno la requiere). Para el overlay VPN, usa también `docker-compose.vpn.yaml` como en `deploy/README.md` del repositorio de desarrollo.
