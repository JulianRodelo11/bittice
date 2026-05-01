# Actualización automática en EC2

Un contenedor **no puede sustituir su propia imagen** de forma fiable sin acceso al **Docker del host** (socket) o a un **orquestador**. Por eso hay dos capas recomendadas:

---

## 1. Watchtower (recomendado para auto‑deploy de imagen)

[Watchtower](https://containrrr.dev/watchtower/) es un contenedor ligero que usa `/var/run/docker.sock`, consulta el **registro** (p. ej. **GHCR**) y **recrea** los servicios cuando el **digest** de la imagen cambia.

**Requisitos:**

- En `.env`, `BITTICE_IMAGE` debe ser una imagen **del registro**, por ejemplo  
  `ghcr.io/julianrodelo11/bittice:v0.1.70`, no solo una etiqueta cargada con `docker load` sin digest remoto.
- Si la imagen es **privada**, configurar login en el host (`docker login ghcr.io`).
- Para actualizar automáticamente al publicar releases: usar **tag que apunte a lo último** (`:latest` si tu CI lo publica) **o** cambiar el tag en `.env` y hacer `compose pull` una vez — Watchtower solo detecta cambios en la referencia que ya tienes en compose.

**Uso en el servidor** (después de tener ya `docker-compose.yaml` + `.env`):

```bash
docker compose -f docker-compose.yaml -f docker-compose.watchtower.yaml --env-file .env up -d
```

El archivo `deploy/docker-compose.watchtower.yaml` está en el repo. El servicio `bittice` lleva la etiqueta  
`com.centurylinklabs.watchtower.enable=true` para que solo ese servicio sea vigilado cuando `WATCHTOWER_LABEL_ENABLE=true`.

Hay **unos segundos de cortes** al recrear el contenedor (normal en una sola máquina).

---

## 2. Aviso en logs (sin reinicio automático)

Si defines **`BITTICE_RELEASE_CHECK_INTERVAL_SECS`** (segundos, mínimo efectivo **60**), el motor consulta la API pública de GitHub **`releases/latest`** y escribe un **`WARN` en logs** cuando el tag de la última release es **semver mayor** que la versión compilada del binario.

- No sustituye la imagen; solo **avisa** en `docker logs`.
- Repo configurable con **`BITTICE_RELEASE_GITHUB_REPO`** (por defecto `JulianRodelo11/bittice`).
- Útil junto con Watchtower o con tu pipeline CI que ya publique imagen nueva.

Ejemplo en compose / `.env`:

```env
BITTICE_RELEASE_CHECK_INTERVAL_SECS=3600
```

---

## 3. Flujo solo `docker load` por SSH

Si despliegas imagen **solo** con `docker save | ssh docker load` y **sin** GHCR, Watchtower **no** puede saber que hay imagen nueva hasta que algo **vuelva a cargar** layers en el host. Para auto‑actualización ahí haría falta un **cron/script en la EC2** que descargue un tarball y haga `docker load && compose up`, o pasar a imagen servida desde **GHCR**.

---

## Resumen

| Objetivo | Mecanismo |
|----------|-----------|
| Que EC2 **sola** ponga la imagen nueva | **Watchtower** + imagen en **GHCR** (u otro registry). |
| Que el motor **avise** sin reiniciar | **`BITTICE_RELEASE_CHECK_INTERVAL_SECS`** → línea en logs. |
| Sin registry | Automatizar **`docker load`** en el host (script/cron/CI), no dentro del contenedor aislado. |
