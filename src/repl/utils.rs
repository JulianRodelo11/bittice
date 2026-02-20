use std::path::Path;

pub fn get_path_suggestions(input: &str) -> Vec<String> {
    // Si está vacío, empezamos en la raíz del sistema
    let raw_query = if input.is_empty() { "/" } else { input };

    // Soporte para ~
    let query = if raw_query.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            raw_query.replacen('~', &home, 1)
        } else {
            raw_query.to_string()
        }
    } else {
        raw_query.to_string()
    };

    let path = Path::new(&query);

    // Determinamos directorio de búsqueda y prefijo
    let (search_dir, prefix) = if query.ends_with(std::path::MAIN_SEPARATOR) {
        (path, "")
    } else if path.parent().is_none() {
        (Path::new("/"), path.to_str().unwrap_or(""))
    } else {
        (
            path.parent().unwrap(),
            path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        )
    };

    let search_dir = if search_dir.as_os_str().is_empty() {
        Path::new("/")
    } else {
        search_dir
    };

    let mut suggestions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Filtrar ocultos y verificar prefijo
            if name.starts_with(prefix) && !name.starts_with('.') {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let is_ndjson = name.ends_with(".ndjson");

                // SOLO mostramos directorios o archivos .ndjson
                if is_dir || is_ndjson {
                    let mut display_path = if raw_query.starts_with('~') {
                        let home = std::env::var("HOME").unwrap_or_default();
                        entry.path().to_string_lossy().replacen(&home, "~", 1)
                    } else {
                        entry.path().to_string_lossy().to_string()
                    };

                    if is_dir {
                        display_path.push(std::path::MAIN_SEPARATOR);
                    }
                    suggestions.push(display_path);
                }
            }
        }
    }
    suggestions.sort();
    suggestions
}

pub fn get_loaded_data() -> Vec<String> {
    let data_path = Path::new("data");
    let mut tree_lines = Vec::new();

    if let Ok(entities) = std::fs::read_dir(data_path) {
        let mut entity_entries: Vec<_> = entities.flatten().collect();
        // Ordenar entidades alfabéticamente
        entity_entries.sort_by_key(|a| a.file_name());

        for entity in entity_entries {
            if let Ok(ft) = entity.file_type() {
                if ft.is_dir() {
                    let entity_name = entity.file_name().to_string_lossy().to_string();

                    // Recolectar tablas
                    let mut tables_list = Vec::new();
                    if let Ok(tables) = std::fs::read_dir(entity.path()) {
                        for table in tables.flatten() {
                            if let Ok(t_ft) = table.file_type() {
                                if t_ft.is_dir() {
                                    tables_list
                                        .push(table.file_name().to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                    tables_list.sort();

                    if !tables_list.is_empty() {
                        // Línea de Entidad con sangría inicial
                        tree_lines.push(format!("  ── {}", entity_name));

                        // Líneas de Tablas con sangría inicial extra
                        for (i, table) in tables_list.iter().enumerate() {
                            let prefix = if i == tables_list.len() - 1 {
                                "     └── "
                            } else {
                                "     ├── "
                            };
                            tree_lines.push(format!("{}{}", prefix, table));
                        }
                    }
                }
            }
        }
    }
    tree_lines
}



pub fn get_indexed_fields(data_path: &Path, entity: &str, table: &str) -> Vec<String> {
    let table_path = data_path.join(entity).join(table);
    
    // 1. Intentar leer desde manifest.json (campos originales)
    let manifest_path = table_path.join("manifest.json");
    if manifest_path.exists() {
        if let Ok(file) = std::fs::File::open(&manifest_path) {
            let reader = std::io::BufReader::new(file);
            if let Ok(manifest) = serde_json::from_reader::<_, serde_json::Value>(reader) {
                if let Some(original_fields) = manifest.get("original_fields").and_then(|f| f.as_array()) {
                    let mut all_fields = std::collections::HashSet::new();
                    
                    let mut date_fields = std::collections::HashSet::new();
                    
                    // Escaneamos el primer segmento para ver qué campos tienen archivos derivados de fecha
                    let segments_dir = table_path.join("segments");
                    if let Ok(entries) = std::fs::read_dir(segments_dir) {
                        if let Some(seg_entry) = entries.filter_map(|e| e.ok()).find(|e| e.path().is_dir()) {
                             if let Ok(files) = std::fs::read_dir(seg_entry.path()) {
                                 for f in files.flatten() {
                                     if let Some(name) = f.file_name().to_str() {
                                         // IGNORAR archivos que empiecen por bitmaps_
                                         if !name.starts_with("bitmaps_") {
                                             if name.ends_with("_day.dat") {
                                                 date_fields.insert(name[..name.len()-8].to_string());
                                             } else if name.ends_with("_month.dat") {
                                                 date_fields.insert(name[..name.len()-10].to_string());
                                             } else if name.ends_with("_year.dat") {
                                                 date_fields.insert(name[..name.len()-9].to_string());
                                             } else if name.ends_with("_hour_bucket.dat") {
                                                 date_fields.insert(name[..name.len()-16].to_string());
                                             }
                                         }
                                     }
                                 }
                             }
                        }
                    }

                    for v in original_fields {
                        if let Some(field) = v.as_str() {
                            let field_s = field.to_string();
                            all_fields.insert(field_s.clone());
                            
                            if date_fields.contains(&field_s) {
                                all_fields.insert(format!("{}_day", field_s));
                                all_fields.insert(format!("{}_month", field_s));
                                all_fields.insert(format!("{}_hour_bucket", field_s));
                                all_fields.insert(format!("{}_year", field_s));
                            }
                        }
                    }
                    
                    let mut result: Vec<String> = all_fields.into_iter().collect();
                    result.sort();
                    return result;
                }
            }
        }
    }

    // 2. Scan segments (Fallback si no hay manifest con original_fields)
    let mut fields = std::collections::HashSet::new();
    let segments_dir = table_path.join("segments");
    if let Ok(entries) = std::fs::read_dir(segments_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Ok(seg_files) = std::fs::read_dir(entry.path()) {
                    for f in seg_files.flatten() {
                        if let Some(name) = f.file_name().to_str() {
                            // Ignorar archivos de metadatos internos
                            if name.ends_with(".dat") && !name.starts_with("bitmaps_") {
                                let field_name = name.trim_end_matches(".dat");
                                // También ignorar si es un derivado oficial para la lista base
                                if !field_name.ends_with("_day") && 
                                   !field_name.ends_with("_month") && 
                                   !field_name.ends_with("_year") && 
                                   !field_name.ends_with("_hour_bucket") {
                                    fields.insert(field_name.to_string());
                                }
                            }
                        }
                    }
                }
                if !fields.is_empty() { break; }
            }
        }
    }

    let mut result: Vec<String> = fields.into_iter().collect();
    result.sort();
    result
}

pub fn get_entities(data_path: &Path) -> Vec<String> {
    let mut entities = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data_path) {
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    entities.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
    }
    entities.sort();
    entities
}

pub fn get_field_values(_data_path: &Path, _entity: &str, _table: &str, _field: &str) -> Vec<String> {
    vec!["Write value".to_string(), "Variable (ask later)".to_string()]
}

pub fn get_order_by_fields(data_path: &Path, entity: &str, table: &str) -> Vec<String> {
    let table_path = data_path.join(entity).join(table);
    let mut date_fields_set = std::collections::HashSet::new();

    // Necesitamos identificar qué campos son REALMENTE fechas
    let segments_dir = table_path.join("segments");
    if let Ok(entries) = std::fs::read_dir(segments_dir) {
        if let Some(seg_entry) = entries.filter_map(|e| e.ok()).find(|e| e.path().is_dir()) {
             if let Ok(files) = std::fs::read_dir(seg_entry.path()) {
                 for f in files.flatten() {
                     if let Some(name) = f.file_name().to_str() {
                         // IGNORAR archivos que empiecen por bitmaps_
                         if !name.starts_with("bitmaps_") {
                             if name.ends_with("_day.dat") {
                                 date_fields_set.insert(name[..name.len()-8].to_string());
                             } else if name.ends_with("_month.dat") {
                                 date_fields_set.insert(name[..name.len()-10].to_string());
                             } else if name.ends_with("_year.dat") {
                                 date_fields_set.insert(name[..name.len()-9].to_string());
                             } else if name.ends_with("_hour_bucket.dat") {
                                 date_fields_set.insert(name[..name.len()-16].to_string());
                             }
                         }
                     }
                 }
             }
        }
    }
    
    let mut date_fields: Vec<String> = date_fields_set.into_iter().collect();
    date_fields.sort();
    date_fields
}

pub fn get_base_fields(all_fields: &[String]) -> Vec<String> {
    // IMPORTANTE: Para la opción "Fields" (columnas a mostrar), solo mostramos originales.
    // Identificamos los originales porque NO terminan en los sufijos derivados.
    let mut filtered: Vec<String> = all_fields.iter()
        .filter(|f| {
            !f.ends_with("_day") && 
            !f.ends_with("_month") && 
            !f.ends_with("_year") && 
            !f.ends_with("_hour_bucket")
        })
        .cloned()
        .collect();
    filtered.sort();
    filtered
}

pub fn get_filtered_fields(all_fields: &[String]) -> Vec<String> {
    // Para filtros, permitimos TODO (originales + derivados)
    let mut filtered = all_fields.to_vec();
    filtered.sort();
    filtered
}
