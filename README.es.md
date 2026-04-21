# Bittice: Motor de Datos de Alto Rendimiento

**Bittice** es un motor de capa de lectura de alto rendimiento escrito en Rust, diseñado para cerrar la brecha entre las bases de datos transaccionales pesadas y la disponibilidad instantánea de los datos.

Proporciona un modelo de almacenamiento columnar ultra rápido que actúa como una réplica en tiempo real, liberando a sus bases de datos principales de cargas masivas de búsqueda y análisis para ahorrar costos de infraestructura y maximizar el rendimiento.

> **Nota:** Bittice está bajo la **Licencia Elastic v2.0**. Es gratuito para uso personal, interno y comercial (ej. para ahorrar costos de infraestructura). Sin embargo, no puedes ofrecerlo a terceros como un servicio gestionado.

## 🚀 La Visión: Sincronización Multi-Fuente
Aunque actualmente cuenta con un conector robusto para **MySQL** vía CDC (Change Data Capture), Bittice está arquitecturado para ser agnóstico a la fuente. Nuestra hoja de ruta incluye sincronización nativa para:
*   🐘 **PostgreSQL** (Próximamente)
*   🗄️ **SQL Server** (Próximamente)
*   🍃 **MongoDB** (Próximamente)

## 🦀 Base Técnica
Bittice está construido para una eficiencia extrema utilizando:
*   **Rust:** Para seguridad de memoria y abstracciones de cero costo.
*   **Tri-File Columnar Mapping (TFCM):** Lógica de almacenamiento propietaria para recuperación O(1).
*   **Memory-Mapped Files (mmap):** Aprovechamiento del page cache del SO para acceso casi instantáneo.
*   **Roaring Bitmaps:** Para filtrado lógico de alta velocidad en conjuntos de datos masivos.

---

## 🛠 Para Desarrolladores: Compilación desde el Código

### Requisitos Previos
*   **Rust & Cargo:** Versión estable más reciente.
*   **Compilador de Protobuf:** Requerido para la compilación de la interfaz gRPC.

### Construir y Ejecutar
```bash
# Clonar el repositorio
git clone https://github.com/julianrodelo/bittice.git
cd bittice

# Construir y ejecutar la CLI interactiva
cargo run
```

---

## 📜 Documentación y Licencia
Para obtener la documentación completa, referencia de la API y guías de instalación, visite nuestro portal de documentación (Próximamente).

### Licencia
Bittice está bajo la **Licencia Elastic v2.0**.

*   **Permitido:** Gratuito para uso personal y uso interno en entornos de producción de cualquier organización para optimizar su propia infraestructura de datos. Puedes modificar y redistribuir el código para fines internos.
*   **Prohibido:** No puedes ofrecer Bittice a terceros como un servicio alojado o gestionado (SaaS), ni puedes vender el software por sí mismo o eliminar los avisos de licencia o derechos de autor.

Consulte [LICENSE](LICENSE) para más detalles.

---
*Creado con pasión por Julian Rodelo.*
