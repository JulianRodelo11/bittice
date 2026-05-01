# Flujo desde cero hasta EC2 (visión general)

Guía **alto nivel** en español: desde que no tienes nada hasta tener Bittice corriendo en una instancia EC2 con tus datos y queries locales empaquetados.

---

## 1. Idea general

1. En tu **PC** instalas y ejecutas Bittice.
2. **Conectas** Bittice a MySQL (RDS u otro), haces la **sincronización CDC** y defines **queries guardadas**.
3. Cuando quieras producción en la nube, desde el **menú Deploy del mismo CLI** construyes una **imagen Docker**, la subes por **SSH** a un **EC2** y sincronizas el árbol **`data/`** (espejos, configs, queries, VPN si aplica).

No todas las conexiones necesitan VPN: solo si el perfil tiene **`vpn_file`** en `cdc_config` (OpenVPN).

---

## 2. Tu PC vs el contenedor en EC2

| Dónde | Al ejecutar `bittice` sin argumentos |
|--------|--------------------------------------|
| **Tu ordenador** | Menú interactivo: conectar y sincronizar desde cero, usar datos ya sincronizados, deploy, salir. |
| **Docker en EC2** | **No hay ese menú**. El contenedor arranca **solo el motor** (APIs + CDC) usando el **`data/`** que ya viene montado desde el deploy. |

La primera configuración CDC, los espejos grandes y las queries las preparas **en local**; el servidor solo **ejecuta** lo persistido en el volumen (`/app/data`). Los `docker-compose` de ejemplo fijan **`BITTICE_ENGINE_ONLY=1`** para garantizar ese comportamiento (incluso si el proceso no es PID 1). Para una sesión puntual de asistente dentro del contenedor existiría **`bittice setup`** explícito (no es el flujo normal de EC2).

---

## 3. Qué necesitas antes de empezar

**En tu máquina**

- Bittice instalado (binario o desde el repo con Rust).
- **Docker** corriendo (para construir la imagen del deploy SSH).
- **OpenSSH** (`ssh`) y una **clave** configurada para entrar al EC2.
- **`rsync`** en tu PC y en el EC2 (el deploy lo usa para `data/` grande).
- **`python3`** y **`bash`** (el script de bundle los usa).

**En AWS**

- Una **instancia EC2** (Linux) con Docker y Docker Compose v2.
- Red/firewall coherentes con tus necesidades (MySQL/RDS accesible desde EC2 o vía VPN).

---

## 4. Paso a paso (local)

### 4.1 Arrancar Bittice por primera vez

1. Ejecutas **`bittice`** (sin argumentos entras al asistente interactivo).
2. Ves el menú principal.

### 4.2 Conectar y sincronizar

1. Eliges **“Connect and sync to a database”**.
2. Introduces host, puerto, usuario y contraseña de MySQL.
3. Indicas si sincronizas **todas las bases del host** o **una sola base**.
4. Si corres dentro de Docker en la nube, el asistente puede preguntarte por **VPN**; en un PC típico suele ser **conexión directa** (sin VPN).
5. Bittice guarda la configuración CDC bajo **`data/`** y arranca la sincronización (binlog / modo estático si no hay CDC).

### 4.3 Usar el motor localmente

1. Cuando ya hay al menos una entidad sincronizada, el menú ofrece **“Use Bittice with synced databases”**.
2. El motor expone la API de **queries guardadas** en el puerto público (**3000**) y la **API admin** (crear/editar queries, config) en **8080** (según la configuración actual del proyecto).

### 4.4 Queries y admin

1. Creas o editas **operaciones guardadas** usando la API admin (**8080**) o las herramientas que uses habitualmente.
2. Todo queda persistido en **`data/`** (junto con espejos CDC, estados, etc.).

---

## 5. VPN (solo si aplica)

**Cuándo:** RDS u otro MySQL solo es alcanzable desde una red privada y necesitas **OpenVPN**.

**Qué hacer:**

1. En el menú **Deploy**, opción para **añadir perfil OpenVPN** (los `.ovpn` quedan en **`data/vpn/`**).
2. Los perfiles **solo son obligatorios en el deploy SSH** si el `cdc_config` de ese perfil tiene **`vpn_file`** apuntando a ese `.ovpn` (por nombre de archivo empaquetado).

**Importante:** Hoy solo se soporta **sin VPN** o **OpenVPN** cuando el perfil lo indica.

---

## 6. Preparar EC2 (una vez)

1. Instalar **Docker** y **Docker Compose v2**.
2. Instalar **`rsync`** en la instancia.
3. Tu usuario debe poder usar **`docker`** (grupo `docker` o equivalente).
4. Abrir en el security group los puertos que vayan a usar **clientes** (p. ej. **3000** para queries públicas; **8080** solo operaciones/admin; **50051** si usan gRPC). Restringir **8080** a redes de confianza es una buena práctica.

---

## 7. Deploy desde el CLI a EC2

1. En el menú principal, entras en **Deploy**.
2. Si usas VPN para algún perfil, antes añades los **.ovpn** necesarios (ver sección 5).
3. Eliges **“Build image + bundle + deploy over SSH (full)”** (o el nombre equivalente en tu versión).
4. Indicas:
   - **Raíz del repo** Bittice (donde está `deploy/Dockerfile.from-source`).
   - **Nombre:etiqueta** de la imagen Docker (la misma en local y en el servidor).
   - **Destino SSH**: `usuario@host` del EC2.
   - **Carpeta remota** bajo el home (p. ej. `bittice-run`).
   - **Arquitectura** de la imagen (amd64 / arm64 / nativa), alineada con el tipo de instancia.

---

## 8. Qué hace ese deploy (muy alto nivel)

1. **Comprueba** herramientas locales y, si hay perfiles con **`vpn_file`**, que los **.ovpn** estén en **`data/vpn/`** o en **`vpn/`** del repo.
2. **Construye** la imagen Docker desde el código del repo (`Dockerfile.from-source`).
3. **Sube la imagen** al EC2 (`docker save` → `ssh docker load`).
4. **Genera un bundle liviano** (compose, `.env`, carpeta **`vpn/`**; el `data/` pesado **no** va duplicado dentro del tarball del bundle).
5. **Copia el bundle** al directorio remoto elegido.
6. **Rsync de `data/`** hacia el EC2 (primera pasada; apto para **muchos GB** y reanudable).
7. **`docker compose up`** en el servidor.
8. **Para** los contenedores un momento, hace **un segundo rsync** de `data/` (delta) para recoger cambios durante subidas largas, y **vuelve a levantar** el servicio.

En el servidor, el `docker-compose` del bundle monta **`./data`** y **`./vpn`**; el contenedor puede levantar OpenVPN si la config lo requiere.

---

## 9. Después del deploy

1. Verificas contenedores: `docker ps`, logs del servicio **bittice**.
2. Los clientes apuntan al **puerto 3000** para ejecutar queries guardadas y al **8080** para administración, según cómo hayas abierto el security group.

---

## 10. Documentación relacionada (inglés / detalle)

- `deploy/README.md` — despliegue con Docker y compose.
- `deploy/SERVER_QUICKSTART.md` — arranque en servidor sin clonar el repo (zip de release).

---

## 11. Resumen en una frase

**En local preparas y sincronizas todo en `data/`; en EC2 el contenedor solo arranca el motor con ese volumen ya poblado — sin menú de “conectar desde cero”.**
