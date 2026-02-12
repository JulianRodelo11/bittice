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

/// Función para ser llamada desde la línea de comandos (sin TUI compleja)
pub fn execute_load_cli(input_path: &str, entity: &str, table: &str) -> Result<()> {
    let output_dir = Path::new("data").join(entity).join(table);
    if output_dir.exists() {
        std::fs::remove_dir_all(&output_dir)?;
    }

    let should_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    
    // Spinner simple de consola
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(indicatif::ProgressStyle::default_spinner().template("{spinner:.green} {msg}")?);
    pb.enable_steady_tick(std::time::Duration::from_millis(100));

    pb.set_message("Pass 1: Analyzing schema...");
    let detected_fields = analyze_schema(input_path, &pb, &should_cancel)?;

    pb.set_message("Pass 2: Writing binary data and indices...");
    process_and_write(input_path, &output_dir, &detected_fields, &pb, &should_cancel)?;

    pb.finish_with_message("✅ Data loaded successfully!");
    Ok(())
}
