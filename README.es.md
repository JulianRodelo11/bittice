# Bittice: Motor de Datos Local de Alto Rendimiento

[Read in English](README.md) | [Leer en Español](README.es.md)

**Bittice** es un motor de datos local de alto rendimiento diseñado para sincronizarse directamente con bases de datos MySQL, sirviendo datos de forma instantánea a través de una CLI interactiva y APIs locales (REST y gRPC). Está diseñado para desarrolladores y empresas que necesitan capas de lectura ultra rápidas para ahorrar costos en la nube y mejorar el rendimiento sin sobrecargar sus bases de datos de producción.

> **Nota:** Bittice es **Source Available** bajo la **Licencia Elastic v2.0**. Es gratuito para uso personal, interno y comercial (ej. para ahorrar costos de infraestructura). Sin embargo, no puedes ofrecerlo a terceros como un servicio gestionado.

## ⚡ Características Clave

*   **Bitmaps Dinámicos:** Utiliza Roaring Bitmaps para operaciones lógicas ultra rápidas (`AND`/`OR`) entre todos los campos de forma dinámica.
*   **Almacenamiento Columnar:** Solo lee los datos necesarios, reduciendo drásticamente la presión de I/O.
*   **Sincronización en Tiempo Real (CDC):** Actúa como una réplica en tiempo real de tu base de datos MySQL usando el Binlog, con impacto cero en el rendimiento de producción.
*   **Preparado para Series Temporales:** Expande automáticamente los campos de fecha en subcolumnas (año, mes, día, etc.) para búsquedas instantáneas.
*   **Joins Multi-Tabla:** Soporta `INNER` y `LEFT` joins en operaciones guardadas.
*   **Agregaciones Avanzadas:** Incluye `GroupBy`, `TopN`, `Avg`, `Min`, `Max` y `CountDistinct` con soporte para `HAVING`.
*   **APIs Flexibles:** Interfaces REST y gRPC de alto rendimiento.

---

## 🚀 Primeros Pasos

### 🛠 Requisitos Previos
*   **Docker & Docker Desktop:** Necesario para contenerizar el motor y el trabajador de sincronización.
*   **Rust (Cargo):** Para construir y ejecutar la CLI interactiva.

### 🏃 Inicio Rápido
Para iniciar Bittice, ejecuta el asistente interactivo:

```bash
cargo run
```

El asistente te guiará a través de:
1.  **Conexión a MySQL:** Solo proporciona tu host, puerto y credenciales.
2.  **Configuración de Entidad:** Elige la base de datos y tablas que deseas indexar.
3.  **Despliegue:** Bittice generará un `docker-compose.yml` para ejecutar el Motor y el Sincronizador.

---

## 🔄 Cómo Funciona

1.  **Bootstrap:** Bittice clona tus datos existentes de MySQL en índices columnares locales altamente optimizados.
2.  **CDC (Change Data Capture):** Escucha el Binlog de MySQL para reflejar operaciones de `INSERT`, `UPDATE` y `DELETE` instantáneamente.
3.  **Consultas:** Defines "Operaciones" (consultas) vía REST o usas el REPL interactivo para obtener datos.

---

## 🛠 Gestión de Consultas (Operaciones)

Las consultas en Bittice se llaman **Operaciones**. Se gestionan a través de la API REST en `/_config`.

### Ejemplo: Crear una Consulta Guardada
```json
{
  "type": "read",
  "details": {
    "name": "ventas_recientes",
    "entity": "sakila",
    "table": "payment",
    "filters": [
      { "field": "amount", "op": ">", "value": "5.00" }
    ],
    "limit": 10,
    "selected_fields": ["*"]
  }
}
```

### Características Avanzadas de Consulta
Bittice soporta:
*   **Consultas Parametrizadas:** Usa `$` (ej. `"value": "$monto_min"`) y pasa valores vía parámetros de URL.
*   **Campos Calculados:** Usa expresiones aritméticas directamente en el `select`.
*   **Agrupación de Respuesta:** Agrupa respuestas REST por claves para estructuras JSON jerárquicas.
*   **Árboles de Filtros:** Construye grupos lógicos anidados complejos (`AND`/`OR`).

---

## 🌐 Referencia de la API

### API REST (Puerto 3000)
*   `GET /nombre_consulta` - Ejecuta una consulta guardada.
*   `GET /nombre_consulta?param=valor` - Ejecuta con parámetros.
*   `POST /_config` - Crea/Actualiza una operación.
*   `GET /_config` - Lista las operaciones.
*   `GET /_entities` - Lista las entidades sincronizadas.

### API gRPC (Puerto 50051)
*   `Search` / `SearchUnary`: Búsquedas ad-hoc en una sola tabla.
*   `ExecuteSavedQuery`: Ejecuta operaciones pre-configuradas.
*   `SubscribeUpdates`: Streaming de cambios de datos en tiempo real.

---

## 📜 Licencia

Bittice está bajo la **Licencia Elastic v2.0**.

*   **Permitido:** Gratuito para uso personal, uso interno en empresas y uso comercial para ahorrar en costos de infraestructura. Puedes modificar y redistribuir el código.
*   **Prohibido:** No puedes ofrecer Bittice a terceros como un servicio alojado o gestionado (SaaS), y no puedes eliminar los avisos de licencia o derechos de autor.

 Para ver los términos completos, consulta el archivo [LICENSE](LICENSE).

---
*Bittice - Rápido, Local, Eficiente.*
