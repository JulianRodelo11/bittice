# Bittice: Motor de Datos Local de Alto Rendimiento

[Read in English](README.md) | [Leer en Español](README.es.md)

**Bittice** es un motor de datos local de alto rendimiento diseñado para sincronizarse directamente con bases de datos MySQL, sirviendo datos de forma instantánea a través de una CLI interactiva y APIs locales (REST y gRPC).

## ⚡ ¿Por qué Bittice? (Rendimiento vs. DBs Tradicionales)

Bittice no es un reemplazo para tu base de datos transaccional principal; es una **capa de lectura de alto rendimiento** diseñada para manejar cargas masivas de búsqueda y análisis que, de otro modo, ralentizarían tu entorno de producción.

### 1. Bitmaps Dinámicos vs. Índices Estáticos
En las bases de datos SQL tradicionales, necesitas índices compuestos específicos (ej. `INDEX(a, b)`) para cada combinación de filtros.
**Bittice utiliza Roaring Bitmaps** para cada valor único. Esto permite al motor realizar operaciones lógicas `AND`/`OR` ultra rápidas entre filtros de forma dinámica, proporcionando flexibilidad total sin el costo de mantener cientos de índices tradicionales.

### 2. Eficiencia Columnar
Las bases de datos tradicionales (orientadas a filas) deben leer filas completas del disco incluso si solo necesitas uno o dos campos.
**Bittice está orientado a columnas.** Solo toca los datos específicos solicitados. Esto reduce drásticamente la presión de I/O y permite procesar millones de registros en milisegundos.

### 3. Aislamiento de Producción (vía CDC)
Ejecutar consultas analíticas pesadas (GroupBys, filtros profundos) en tu base de datos de producción puede causar bloqueos y ralentizar a los usuarios.
**Bittice utiliza Change Data Capture (CDC)** para actuar como una réplica aislada en tiempo real. Puedes ejecutar cargas de búsqueda intensivas en Bittice con **impacto cero** en el rendimiento de tu base de datos principal.

### 4. Enriquecimiento Nativo de Series Temporales
En lugar de calcular el año, mes o día durante la consulta (que es lento), **Bittice expande automáticamente los campos de fecha** en subcolumnas durante la ingesta. Esto transforma los costosos cálculos de fecha en búsquedas instantáneas en índices.

---

## 🦀 Construido con Rust

Bittice está escrito completamente en **Rust**, lo cual es crucial para su rendimiento y fiabilidad:

- **Abstracciones de Cero Costo:** Código de alto nivel que se compila en instrucciones de máquina eficientes sin la sobrecarga de un recolector de basura (GC).
- **Seguridad de Memoria:** El modelo de propiedad de Rust garantiza la seguridad de la memoria y evita errores comunes como punteros nulos o carreras de datos.
- **Alta Concurrencia:** Utilizando `Tokio` y `Rayon`, Bittice paraleliza las búsquedas y la materialización de datos en todos los núcleos del CPU con una sobrecarga mínima.
- **Acceso Directo al Sistema:** Rust permite un control detallado sobre los archivos mapeados en memoria (`mmap`), permitiendo al motor manejar conjuntos de datos mucho más grandes que la RAM disponible dejando que el SO gestione el cache de páginas.

---

## 🛠 Requisitos Previos

Antes de comenzar, asegúrate de tener instalado lo siguiente:
- **Docker & Docker Desktop:** Obligatorio. Bittice utiliza Docker para contenerizar el motor y el trabajador de sincronización.
- **Rust (Cargo):** Para ejecutar el proyecto localmente.

---

## 🚀 Primeros Pasos

Para iniciar Bittice, simplemente ejecuta el proyecto. El asistente interactivo te guiará en la configuración:

```bash
cargo run
```

Este comando único te ofrece dos caminos claros:
1. **Conectar y sincronizar:** Configura una nueva conexión MySQL para iniciar una sincronización CDC en tiempo real.
2. **Usar datos existentes:** Salta directamente al motor de consultas utilizando datos ya sincronizados.

---

## 🔄 Paso a Paso: Conexión a MySQL

Cuando elijas **"Connect and synchronize"**, sigue estos pasos:

1.  **MySQL Host:** Introduce la dirección de tu base de datos (ej. `localhost` o `192.168.1.100`).
2.  **Puerto:** El puerto donde escucha MySQL (normalmente `3306`).
3.  **Usuario y Contraseña:** Tus credenciales de la base de datos.
4.  **Base de datos a sincronizar:** El nombre de la base de datos específica que deseas indexar.
5.  **Nombre de la Entidad:** Un apodo para esta conexión en Bittice (usado en las rutas de tu API).
6.  **Sincronización Inicial:** Bittice iniciará un "Bootstrap" para clonar tus datos existentes en índices locales.
7.  **Construcción de Imagen Docker:** El asistente te pedirá construir una imagen Docker personalizada para tu entidad. **Esto es altamente recomendado.**
8.  **Stack de Docker Compose:** Finalmente, te ofrecerá generar e iniciar un `docker-compose.yml`. Esto crea dos contenedores:
    -   `engine`: El servidor de consultas (REST/gRPC).
    -   `sync`: El trabajador que mantiene los datos actualizados en tiempo real usando el Binlog de MySQL.

---

## 🔄 Sincronización MySQL (CDC)

Bittice actúa como una réplica en tiempo real de tu base de datos MySQL. Una vez completada la sincronización inicial, escucha el registro binario de MySQL (Binlog) para reflejar operaciones de `INSERT`, `UPDATE` y `DELETE` instantáneamente en tus índices locales.

**Soporte Offline:** Si la sincronización se detiene, se reanuda desde el último estado conocido al reiniciar.

---

## 🛠 Gestión de Consultas

Las consultas en Bittice se llaman **Operaciones**. Puedes gestionarlas usando la API REST en el endpoint `/_config`.

### Crear una Consulta (POST)
Envía una solicitud `POST` a `http://localhost:3000/_config` con la definición de la consulta:

```json
{
  "type": "read",
  "details": {
    "name": "ventas_recientes",
    "entity": "sakila",
    "table": "payment",
    "filters": [
      { "field": "amount", "op": "Gt", "value": "5.00" }
    ],
    "order_by": [{ "field": "payment_date", "direction": "Desc" }],
    "limit": 10,
    "selected_fields": ["*"]
  }
}
```

### Listar Consultas (GET)
`GET http://localhost:3000/_config`

### Eliminar una Consulta (DELETE)
`DELETE http://localhost:3000/_config?name=ventas_recientes`

### Crear una Consulta Multi-Tabla (POST)
Las operaciones `read` ahora pueden unir varias tablas sin cambiar el formato actual de una sola tabla. La consulta multi-tabla mantiene `table` como tabla base y añade `table_alias`, `joins` y `select`.

```json
{
  "type": "read",
  "details": {
    "name": "sesiones_con_usuarios",
    "entity": "goparking",
    "table": "Sessions",
    "table_alias": "s",
    "joins": [
      {
        "type": "Inner",
        "table": "Users",
        "alias": "u",
        "on": [
          { "left": "s.userId", "op": "Eq", "right": "u.id" }
        ]
      }
    ],
    "filters": [
      { "field": "s.status", "op": "Eq", "value": "OPEN" },
      { "field": "u.document", "op": "Eq", "value": "$document" }
    ],
    "select": [
      { "field": "s.id", "as": "session_id" },
      { "field": "s.plate", "as": "plate" },
      { "field": "u.name", "as": "user_name" }
    ],
    "order_by": [{ "field": "s.createdAt", "direction": "Desc" }],
    "limit": 50
  }
}
```

Alcance actual para operaciones multi-tabla:

- Solo `INNER` y `LEFT JOIN`.
- Solo joins por igualdad (`Eq` dentro de `on`).
- Queries guardadas por REST y `ExecuteSavedQuery` / `ExecuteSavedQueryUnary` en gRPC.
- `Search` / `SearchUnary` directos siguen siendo de una sola tabla para preservar el contrato actual.

### Agrupar la Respuesta REST por una Clave
Si quieres que la respuesta REST no llegue plana, sino agrupada por una clave, puedes usar `response_grouping`. Esto es útil para obtener estructuras como `parqueaderoId -> horarios_por_dia`.

```json
{
  "type": "read",
  "details": {
    "name": "horarios_agrupados_parqueaderos",
    "entity": "inside",
    "table": "ParqueaderoHorario",
    "table_alias": "ph",
    "joins": [
      {
        "type": "Inner",
        "table": "Dia",
        "alias": "d",
        "on": [
          { "left": "ph.diaId", "op": "Eq", "right": "d.diaId" }
        ]
      }
    ],
    "filters": [
      { "field": "d.esActivo", "op": "Eq", "value": "1" }
    ],
    "filters_op": "And",
    "order_by": [
      { "field": "ph.parqueaderoId", "direction": "Asc" },
      { "field": "ph.diaId", "direction": "Asc" }
    ],
    "select": [
      { "field": "ph.parqueaderoId", "as": "parqueaderoId" },
      { "field": "ph.diaId", "as": "diaId" },
      { "field": "ph.horaApertura", "as": "horaApertura" },
      { "field": "ph.horaCierre", "as": "horaCierre" },
      { "field": "d.nombre", "as": "diaNombre" },
      { "field": "d.abreviatura", "as": "diaAbreviatura" }
    ],
    "response_grouping": {
      "field": "parqueaderoId",
      "items_as": "horarios_por_dia"
    }
  }
}
```

La respuesta REST quedará así:

```json
{
  "data": [
    {
      "parqueaderoId": 5,
      "horarios_por_dia": [
        {
          "diaId": 2,
          "horaApertura": "05:43",
          "horaCierre": "08:43",
          "diaNombre": "Lunes",
          "diaAbreviatura": "L"
        },
        {
          "diaId": 3,
          "horaApertura": "05:43",
          "horaCierre": "08:43",
          "diaNombre": "Martes",
          "diaAbreviatura": "M"
        }
      ]
    }
  ]
}
```

Notas sobre `response_grouping`:

- Solo aplica a respuestas REST de operaciones guardadas.
- Agrupa usando el nombre de campo ya proyectado en `select` o `selected_fields`.
- Soporta `field` para un solo campo o `fields` para promover varios campos al objeto padre.
- Soporta `children` para agrupación jerárquica de varios niveles.
- Por defecto elimina el campo o los campos de agrupación de cada item interno.
- Cuando se usa, Bittice intenta reunir todas las filas necesarias para devolver la respuesta agrupada y omite `pagination`.
- Por seguridad, la respuesta agrupada está limitada a `10000` filas fuente.

Ejemplo con varios campos en el padre:

```json
"response_grouping": {
  "fields": ["parqueaderoId", "parqueaderoNombre"],
  "items_as": "horarios_por_dia"
}
```

Ejemplo con dos niveles de agrupación:

```json
"response_grouping": {
  "fields": ["clienteId", "clienteNombre"],
  "items_as": "cuentas",
  "children": [
    {
      "fields": ["cuentaId", "cuentaNumero"],
      "items_as": "movimientos"
    }
  ]
}
```

Eso permite que una proyección plana como `clienteId, clienteNombre, cuentaId, cuentaNumero, movimientoId, fecha, valor` vuelva por REST como `cliente -> cuentas -> movimientos`.

### Agregación Collect para Arreglos Anidados
Si quieres mantener la respuesta principal REST plana o paginada, pero además devolver una estructura agrupada y anidada como agregación, usa `Collect`.

Por ahora `Collect` es solo REST. Opera sobre la forma ya proyectada de la respuesta, así que `group_by`, `group_by_fields`, `item_fields` y su `order_by` interno deben referirse a nombres de salida definidos en `select` o `selected_fields`.

```json
{
  "type": "read",
  "details": {
    "name": "parking_schedules_collect",
    "entity": "inside",
    "table": "ParqueaderoHorario",
    "table_alias": "ph",
    "joins": [
      {
        "type": "Inner",
        "table": "Dia",
        "alias": "d",
        "on": [
          { "left": "ph.diaId", "op": "Eq", "right": "d.diaId" }
        ]
      }
    ],
    "select": [
      { "field": "ph.parqueaderoId", "as": "parqueaderoId" },
      { "field": "ph.diaId", "as": "diaId" },
      { "field": "ph.horaApertura", "as": "horaApertura" },
      { "field": "ph.horaCierre", "as": "horaCierre" },
      { "field": "d.nombre", "as": "diaNombre" },
      { "field": "d.abreviatura", "as": "diaAbreviatura" }
    ],
    "aggregations": [
      {
        "Collect": {
          "group_by": "parqueaderoId",
          "items_as": "horarios_por_dia",
          "item_fields": ["diaId", "horaApertura", "horaCierre", "diaNombre", "diaAbreviatura"],
          "order_by": [
            { "field": "diaId", "direction": "Asc" }
          ]
        }
      }
    ]
  }
}
```

La carga agregada se devuelve dentro de `aggregations`:

```json
{
  "aggregations": [
    {
      "kind": "Collect",
      "items_as": "horarios_por_dia",
      "summary": 2,
      "data": [
        {
          "parqueaderoId": 5,
          "horarios_por_dia": [
            {
              "diaId": 2,
              "horaApertura": "05:43",
              "horaCierre": "08:43",
              "diaNombre": "Lunes",
              "diaAbreviatura": "L"
            },
            {
              "diaId": 3,
              "horaApertura": "05:43",
              "horaCierre": "08:43",
              "diaNombre": "Martes",
              "diaAbreviatura": "M"
            }
          ]
        }
      ]
    }
  ]
}
```

Notas sobre `Collect`:

- Es opt-in y no cambia la respuesta principal `data` salvo que también uses `response_grouping`.
- Actualmente funciona solo en queries guardadas vía REST.
- Recupera todas las filas fuente necesarias para construir la agregación y usa el mismo límite de seguridad de `10000` filas fuente que `response_grouping`.
- Usa `group_by_fields` cuando necesites más de un campo padre.
- Si omites `item_fields`, Bittice incluye todos los campos proyectados excepto los campos padre de agrupación.

### Filtros Avanzados
Las queries guardadas ahora soportan operadores más expresivos en `filters`, además de un `filter_tree` opcional para queries guardadas multi-tabla.

Operadores soportados en queries guardadas:

- `Between`
- `NotIn`
- `IsNull`
- `IsNotNull`
- `Contains`
- `StartsWith`
- `EndsWith`
- Los operadores existentes como `Eq`, `Ne`, `Gt`, `Gte`, `Lt`, `Lte` e `In`

Ejemplo usando operadores avanzados en una lista plana de filtros:

```json
"filters": [
  { "field": "estado", "op": "Eq", "value": "ACTIVO" },
  { "field": "fecha", "op": "Between", "value": "2026-04-01", "value_to": "2026-04-30" },
  { "field": "canal", "op": "NotIn", "values": ["LEGACY", "BATCH"] },
  { "field": "nombre", "op": "Contains", "value": "premium" },
  { "field": "deletedAt", "op": "IsNull", "value": "" }
],
"filters_op": "And"
```

Ejemplo usando grupos lógicos anidados con `filter_tree`:

```json
"filter_tree": {
  "op": "Or",
  "filters": [
    {
      "op": "And",
      "filters": [
        { "field": "c.estado", "op": "Eq", "value": "ACTIVO" },
        { "field": "m.fecha", "op": "Between", "value": "2026-04-01", "value_to": "2026-04-30" }
      ]
    },
    {
      "op": "And",
      "filters": [
        { "field": "c.esVip", "op": "Eq", "value": "1" },
        { "field": "m.saldo", "op": "Gt", "value": "100000" }
      ]
    }
  ]
}
```

Notas sobre filtros avanzados:

- Los operadores avanzados en `filters` quedan soportados en los paths actuales de ejecución de queries guardadas single-table y multi-table.
- `filter_tree` es opt-in y por ahora se evalúa en el executor multi-table de queries guardadas.
- `Search` y `SearchUnary` ad-hoc por gRPC siguen usando el enum actual del proto, así que estos operadores nuevos todavía no se exponen allí.

### Campos Calculados en Select
Las queries guardadas multi-tabla ahora soportan proyecciones calculadas directamente dentro de `select` usando `expression`.

Ejemplo:

```json
"select": [
  { "field": "m.cargo", "as": "cargo" },
  { "field": "m.abono", "as": "abono" },
  { "expression": "m.cargo - m.abono", "as": "saldo" },
  { "expression": "IF(m.cargo > m.abono, 1, 0)", "as": "enDeuda" }
],
"order_by": [
  { "field": "saldo", "direction": "Desc" }
]
```

Notas sobre campos calculados:

- Este primer corte queda soportado en el executor multi-tabla de queries guardadas.
- Las expresiones reutilizan el parser actual de expresiones aritméticas y `IF(...)` de Bittice.
- Hoy las expresiones son numéricas, así que cubren bien saldos, diferencias, razones y banderas condicionales simples.
- Puedes ordenar por el alias calculado en `order_by`.

### Agregaciones Analíticas
Las queries guardadas multi-tabla ahora también soportan `Avg`, `Min`, `Max` y `CountDistinct`, además de una cláusula opcional `having` para filtrar la salida agrupada.

Ejemplo:

```json
"aggregations": [
  {
    "Avg": {
      "expression": "m.cargo - m.abono",
      "group_by": "c.ciudad",
      "having": { "op": "Gt", "value": "1000" }
    }
  },
  {
    "CountDistinct": {
      "field": "m.facturaId",
      "group_by": "c.ciudad",
      "having": { "op": "Gte", "value": "3" }
    }
  },
  {
    "Max": {
      "field": "m.saldo"
    }
  }
]
```

Notas sobre agregaciones analíticas:

- `Avg`, `Min` y `Max` aceptan `field` o `expression`.
- `CountDistinct` por ahora requiere `field`.
- `having` es opcional y solo aplica a salidas agrupadas como `GroupBy`, `TopN`, `Sum`, `Avg`, `Min`, `Max` y `CountDistinct`.
- `having` reutiliza los operadores de comparación ya soportados en saved queries como `Eq`, `Gt`, `Gte`, `Lt`, `Lte`, `Between` e `In`.
- Este primer corte queda implementado en el executor multi-tabla de queries guardadas.

---

## 🌐 Referencia de la API

### API REST (Puerto 3000)

- **Ejecutar Consulta:** `GET /nombre_consulta`
- **Consulta Parametrizada:** `GET /nombre_consulta?param1=valor1`
  - *Nota: Usa el prefijo `$` en tu definición de consulta (ej. `"value": "$monto_min"`) para convertirlo en parámetro.*
- **Información del Sistema:** `GET /_debug`, `GET /_entities`

### API gRPC (Puerto 50051)

Bittice proporciona una interfaz gRPC de alto rendimiento definida en `proto/bittice.proto`.

- **`Search` / `SearchUnary`:** Búsqueda ad-hoc directa sobre una sola tabla.
- **`ExecuteSavedQuery`:** Ejecuta una operación preconfigurada por nombre.
- **`SubscribeUpdates` (Tiempo real):** Transmisión de actualizaciones para una tabla específica. Recibe notificaciones instantáneas cuando los datos cambian.

---

## 📁 Estructura de Datos y Puertos

- **Puerto REST por defecto:** `3000`
- **Puerto gRPC por defecto:** `50051`
- **Ruta de Almacenamiento:** Todos los datos indexados se guardan en el directorio `data/`.
- **Operaciones:** Las consultas guardadas se persisten en `data/.bittice_ops.json`.

---
*Bittice - Rápido, Local, Eficiente.*
