use std::path::Path;
use anyhow::Result;
use dialoguer::{Input, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use console::style;

use crate::core::schema::analyze_schema;
use crate::core::writer::process_and_write;

// Función para ser llamada desde el modo TUI
pub fn execute_load_tui(input_path: &str, entity: &str, table: &str) -> Result<()> {
    // Definir carpeta de salida: data/{entity}/{table}
    let output_dir = Path::new("data").join(entity).join(table);
    if output_dir.exists() {
        std::fs::remove_dir_all(&output_dir)?;
    }
    
    // Configurar Spinner para Análisis
    let spinner = ProgressBar::new_spinner();
    //spinner.set_style(ProgressStyle::default_spinner()
    //    .template("{spinner:.green} {msg}")?);
    //spinner.set_message("Analizando esquema (Pasada 1)...");

    // Ejecutar Pasada 1
    let detected_fields = analyze_schema(input_path, &spinner)?;
    //spinner.finish_with_message(format!("Análisis completado. Campos detectados: {}", detected_fields.len()));

    // Configurar Barra para Escritura
    let pb = ProgressBar::new_spinner(); // O usa .new(total_bytes) si supieras el tamaño
    //pb.set_style(ProgressStyle::default_spinner()
    //    .template("{spinner:.blue} {msg} {pos} registros procesados")?);
    //pb.set_message("Escribiendo archivos (Pasada 2)...");

    // Ejecutar Pasada 2
    process_and_write(input_path, &output_dir, &detected_fields, &pb)?;
    
    //pb.finish_with_message("¡Carga completada exitosamente!");
    
    //println!("{}", style(format!("Datos guardados en: {}", output_dir.display())).green());
    Ok(())
}
