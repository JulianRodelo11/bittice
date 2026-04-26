# Despliegue de Bittice (Docker)

Este directorio concentra la definición de imagen, compose y scripts para publicar o ejecutar Bittice en un servidor (por ejemplo EC2) con Docker, sin requerir toolchains en el host de producción.

## Contenido

| Archivo | Uso |
|--------|-----|
| `Dockerfile` | Imagen mínima Debian que copia un **binario Linux** ya compilado (igual que el job `docker` de CI). |
| `Dockerfile.from-source` | Compila el motor con `cargo` dentro de la imagen; útil para pruebas sin cross-compile. |
| `docker-compose.production.yaml` | Servicio con reinicio automático, puertos 3000/50051, volumen nombrado para `/app/data`. |
| `docker-compose.vpn.yaml` | Overlay opcional (OpenVPN, `/dev/net/tun`, volumen de `.ovpn`). |
| `scripts/build-image-from-source.sh` | `docker build` usando `Dockerfile.from-source`. |
| `scripts/build-image-from-binary.sh` | `docker build` igual que en release, a partir de un `bittice` Linux. |
| `ec2/user-data.example.sh` | Boceto de arranque en instancia (Docker + `docker run`). |
| `.env.example` | Variables para compose (copia a `deploy/.env`). |

## Imagen en GitHub Container Registry

Al publicar un tag `v*` en este repositorio, el workflow *Release* compila los binarios, construye imágenes `linux/amd64` y `linux/arm64` y publica un manifiesto multi-arquitectura en:

`ghcr.io/<propietario>/bittice:<tag>`

Sustituye el tag y el propietario en `BITTICE_IMAGE` dentro de `deploy/.env`.

## Desplegar sin clonar el repo (solo GitHub Actions / release)

En cada release se adjunta **`bittice-server-<tag>.zip`**: incluye `docker-compose.yaml`, `docker-compose.vpn.yaml`, un `.env` con `BITTICE_IMAGE` ya apuntando a la imagen de ese tag, y **`SERVER_QUICKSTART.md`** (copiado como `README.md` dentro del zip). Descarga el zip desde la página del release, súbelo al servidor y sigue las instrucciones del README del zip. No hace falta tener el código fuente en la instancia.

## Arranque en un servidor (compose)

En la instancia: instala Docker (y *Compose v2*). Copia al servidor el directorio `deploy/` o solo `docker-compose.production.yaml` y un `.env`.

```bash
cd deploy
cp .env.example .env
# edita .env: BITTICE_IMAGE, puertos, BITTICE_HOST, etc.
docker compose -f docker-compose.production.yaml --env-file .env up -d
```

Compose lee `BITTICE_IMAGE` y los demás `BITTICE_*` del archivo; si no usas `.env`, exporta al menos `BITTICE_IMAGE` en el entorno o pasa `--env-file` con otra ruta.

Los datos viven en el volumen Docker `bittice-data` (no se pierden al recrear el contenedor salvo que borres el volumen).

**VPN:** si necesitas el mismo patrón que el instalador en la nube (OpenVPN en contenedor):

```bash
cd deploy
docker compose -f docker-compose.production.yaml -f docker-compose.vpn.yaml up -d
```

Crea en el host el directorio `deploy/vpn` (o define `BITTICE_VPN_HOST_DIR`) y coloca los `.ovpn` ahí.

## Construir la imagen localmente

**Desde el código (Rust dentro de Docker):**

```bash
./deploy/scripts/build-image-from-source.sh
```

**Desde un binario Linux ya construido** (mismo criterio que CI):

```bash
./deploy/scripts/build-image-from-binary.sh path/al/bittice
```

**Subir a Amazon ECR** (después de `aws ecr get-login password` o `ecr get-login` según el cliente):

```bash
docker tag bittice:local 123456789012.dkr.ecr.us-east-1.amazonaws.com/mi-bittice:v0.1.0
docker push 123456789012.dkr.ecr.us-east-1.amazonaws.com/mi-bittice:v0.1.0
```

En EC2, usa la URI de ECR en `BITTICE_IMAGE` y configura el rol/instancia para `ecr:BatchGetImage` y `ecr:GetDownloadUrlForLayer` si aplicas políticas mínimas.

## Variables de entorno frecuentes

Definidas en Bittice (ver código): `BITTICE_HOST` (p. ej. `0.0.0.0`), `BITTICE_DISABLE_CDC_AUTOSTART`, `BITTICE_ENTITY`, variables de directorio VPN, etc. Crédenciales de MySQL no deben ir en la imagen; residen bajo el volumen de datos `/app/data` según el flujo de configuración de CDC.

## Licencia

El uso de Bittice en tu propia infraestructura está sujeto a *Elastic License 2.0* (ver repositorio). No ofrecer Bittice como servicio alojado a terceros salvo lo permitido por la licencia.
