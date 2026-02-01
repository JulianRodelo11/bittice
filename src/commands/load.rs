use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::core::schema::analyze_schema;
use crate::core::writer::process_and_write;
use crate::ui::spinner::run_with_spinner;

// Función para ser llamada desde el modo TUI
pub fn execute_load_tui(
    input_path: &str,
    entity: &str,
    table: &str,
    start_x: u16,
    start_y: u16,
) -> Result<()> {
    // Definir carpeta de salida: data/{entity}/{table}
    let output_dir = Path::new("data").join(entity).join(table);
    if output_dir.exists() {
        std::fs::remove_dir_all(&output_dir)?;
    }

    // Mensaje inicial

    let initial_msg = "Analyzing schema and detecting field types (Pass 1 of 2)...";

    // Ejecutar con el componente reutilizable

    run_with_spinner(
        initial_msg,
        start_y,
        start_x, // Usamos start_x como indentación
        |spinner, should_cancel| {
            // --- Pasada 1: Análisis ---

            let detected_fields = analyze_schema(input_path, spinner, should_cancel)?;

            // Verificar cancelación entre pasos

            if should_cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("Operation cancelled by user"));
            }

            // --- Pasada 2: Escritura ---

            spinner.set_message("Processing records and generating index files (Pass 2 of 2)...");

            process_and_write(
                input_path,
                &output_dir,
                &detected_fields,
                spinner,
                should_cancel,
            )?;

            Ok(())
        },
    )
}
