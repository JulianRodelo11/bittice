# Bittice: Local Data Engine

**Bittice** es un motor de búsqueda y análisis de datos local de alto rendimiento diseñado para procesar archivos NDJSON masivos y servirlos de forma instantánea a través de una interfaz interactiva (TUI) y una API local.

## ¿Qué es Bittice?

Bittice es una herramienta de ingeniería de datos "todo en uno" que permite transformar archivos de texto plano (JSON Lines) en una estructura binaria optimizada para consultas ultra rápidas. Su enfoque principal es la **localidad** y la **velocidad**, eliminando la necesidad de configurar bases de datos complejas para tareas de exploración y servicio de datos.

## Arquitectura a Alto Nivel

El sistema se divide en tres componentes principales:

1.  **Core Engine (Escritura y Consulta):**
    *   **Indexación:** Transforma JSON en archivos binarios de acceso aleatorio (`.dat`) e índices de posición (`.offsets`).
    *   **Bitmaps:** Utiliza mapas de bits (Roaring Bitmaps) para realizar filtrados complejos en milisegundos, incluso con millones de registros.
    *   **Componentes de Tiempo:** Detecta fechas automáticamente y genera sub-campos para agrupar por día, mes u hora.

2.  **Interactive TUI (REPL):**
    *   Una interfaz visual en la terminal construida con `ratatui`.
    *   Permite cargar datos, configurar filtros, definir agregaciones (como TopN o GroupBy) y previsualizar resultados en tiempo real.
    *   **Gestión de Queries:** Las consultas se pueden guardar, editar y cargar con atajos de teclado (`Shift+L`, `Shift+E`, `Shift+S`).

3.  **Local API Server:**
    *   Un servidor web integrado (basado en `Axum`) que expone las consultas guardadas como endpoints HTTP.
    *   Soporta **Consultas Parametrizadas**: puedes definir variables en tus filtros (ej: `$plate`) y pasarlas como parámetros en la URL (`?plate=XYZ`).

## Flujo de Trabajo

1.  **Load:** Se importa un archivo `.ndjson`. Bittice analiza el esquema, detecta tipos y crea los archivos binarios e índices en la carpeta `data/`.
2.  **Search:** El usuario utiliza la TUI para explorar los datos, aplicando filtros y ordenamientos sobre los índices creados.
3.  **Save:** Las configuraciones de búsqueda útiles se guardan con un nombre único.
4.  **Serve:** El servidor local detecta estas queries guardadas y las pone a disposición de cualquier cliente HTTP en `127.0.0.1:3000`.

## Características Clave

*   **Zero-Copy Sorting:** Ordenamiento eficiente directamente sobre archivos mapeados en memoria (mmap).
*   **Parameterized Queries:** Flexibilidad total para reutilizar búsquedas cambiando valores dinámicamente.
*   **Bajo Consumo:** Diseñado para ser extremadamente ligero en recursos mientras mantiene un rendimiento de lectura superior a bases de datos documentales tradicionales para casos de uso local.

---
*Bittice - Fast, Local, Efficient.*
