# Propuesta Técnica: Bittice Mutable (CRUD Support)

Esta propuesta detalla la evolución de **Bittice** de un motor de solo lectura a una base de datos embebida con soporte completo para operaciones de creación, edición y eliminación (CRUD), manteniendo el alto rendimiento de lectura actual.

## 1. Filosofía de Diseño
Para evitar la degradación del rendimiento por fragmentación de archivos binarios, se propone un enfoque basado en **LSM-Tree simplificado** y **Bitmaps de Anulación**.

## 2. Implementación de Operaciones

### A. Eliminación (Delete) vía *Tombstone Bitmaps*
En lugar de reescribir archivos binarios pesados al borrar un registro:
*   **Mecanismo:** Se crea un archivo global por tabla: `index/deleted_ids.bitmap`.
*   **Funcionamiento:** Al eliminar un registro, su `internal_id` se marca en este bitmap.
*   **Consulta:** El motor de búsqueda restará automáticamente estos IDs de cualquier resultado (`active_ids = target_ids AND NOT deleted_ids`).

### B. Creación (Create) vía *Append-Only*
*   **Mecanismo:** Los `FieldWriters` permitirán la apertura en modo "Append".
*   **Funcionamiento:** Cada nuevo registro recibe el siguiente `internal_id` disponible y se escribe al final de los archivos `.dat` y `.offsets`.
*   **Indexación:** Los mapas de bits de búsqueda (`bitmaps_{campo}.dat`) se actualizan en memoria y se persisten, añadiendo el nuevo ID al valor correspondiente.

### C. Actualización (Update) vía *Copy-on-Write*
Dado que los valores binarios tienen tamaños fijos en el archivo `.dat`:
*   **Mecanismo:** `Update = Delete + Create`.
*   **Funcionamiento:** 
    1. El ID del registro antiguo se marca en el `deleted_ids.bitmap`.
    2. El registro actualizado se inserta como uno nuevo al final de la base de datos.
*   **Ventaja:** Permite implementar control de versiones (MVCC) y facilita la recuperación ante fallos.

## 3. Mantenimiento: Proceso de Compactación
Para limpiar los registros marcados como eliminados y recuperar espacio en disco:
*   **Compaction:** Un proceso periódico que lee los archivos actuales, filtra los IDs marcados en el `deleted_ids.bitmap`, reasigna IDs contiguos y genera nuevos archivos `.dat` y `.offsets` optimizados.

## 4. Interfaces de Usuario

### TUI (Terminal Interface)
*   **Data Entry:** Nueva pantalla con formulario dinámico basado en el esquema detectado.
*   **Acciones Directas:** En la tabla de resultados, atajos para:
    *   `d`: Eliminar fila seleccionada.
    *   `e`: Editar fila (abre un buffer de texto con el JSON).

### API Server
*   **POST** `/{entity}/{table}`: Inserción de nuevos documentos.
*   **PUT/PATCH** `/{entity}/{table}/{id}`: Actualización parcial o total.
*   **DELETE** `/{entity}/{table}/{id}`: Eliminación lógica.

## 5. Ventajas de este Enfoque
1.  **Velocidad de Escritura:** Las inserciones son extremadamente rápidas al ser secuenciales (append-only).
2.  **Integridad de Datos:** Menor riesgo de corrupción que la escritura aleatoria (in-place).
3.  **Baja Latencia de Lectura:** El uso de `RoaringBitmaps` para gestionar eliminaciones mantiene las consultas en el rango de microsegundos.

---
*Propuesta generada para la evolución del motor Bittice.*
