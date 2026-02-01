use serde::Deserialize;
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
    } else {
        if path.parent().is_none() {
            (Path::new("/"), path.to_str().unwrap_or(""))
        } else {
            (
                path.parent().unwrap(),
                path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            )
        }
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

#[derive(Deserialize)]
struct Config {
    indexed_fields: Vec<IndexedField>,
}

#[derive(Deserialize)]
struct IndexedField {
    field_name: String,
    indexed: bool,
}

pub fn get_indexed_fields(data_path: &Path, entity: &str, table: &str) -> Vec<String> {
    let config_path = data_path.join(entity).join(table).join("config.json");

    let mut fields = Vec::new();

    if let Ok(content) = std::fs::read_to_string(config_path) {
        if let Ok(config) = serde_json::from_str::<Config>(&content) {
            for item in config.indexed_fields {
                if item.indexed {
                    fields.push(item.field_name);
                }
            }
        }
    }

    fields.sort();
    fields
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_get_path_suggestions() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Create dummy files and directories
        fs::create_dir(dir_path.join("subdir")).unwrap();
        fs::File::create(dir_path.join("file1.ndjson")).unwrap();
        fs::File::create(dir_path.join("file2.txt")).unwrap();
        fs::create_dir(dir_path.join(".hidden_dir")).unwrap();
        fs::File::create(dir_path.join(".hidden_file")).unwrap();

        // Test case 1: Empty input, should suggest from root
        // Note: This test is environment-dependent, so we'll test relative paths

        // Test case 2: Suggest directories and .ndjson files
        let mut input = dir_path.to_str().unwrap().to_string();
        input.push('/');
        let suggestions = get_path_suggestions(&input);
        assert!(suggestions.iter().any(|s| s.ends_with("subdir/")));
        assert!(suggestions.iter().any(|s| s.ends_with("file1.ndjson")));
        assert!(!suggestions.iter().any(|s| s.ends_with("file2.txt")));
        assert!(!suggestions.iter().any(|s| s.ends_with(".hidden_dir/")));
        assert!(!suggestions.iter().any(|s| s.ends_with(".hidden_file")));

        // Test case 3: Suggest with a prefix
        let path_buf = dir_path.join("f");
        let prefix_input = path_buf.to_str().unwrap();
        let suggestions_prefix = get_path_suggestions(prefix_input);
        assert!(suggestions_prefix.iter().any(|s| s.ends_with("file1.ndjson")));
        assert!(!suggestions_prefix.iter().any(|s| s.ends_with("subdir/")));
    }

    #[test]
    fn test_get_indexed_fields() {
        let dir = tempdir().unwrap();
        let data_path = dir.path();
        let entity = "test_entity";
        let table = "test_table";
        let config_dir = data_path.join(entity).join(table);
        fs::create_dir_all(&config_dir).unwrap();

        let config_content = r#"
        {
            "indexed_fields": [
                {"field_name": "id", "indexed": true},
                {"field_name": "name", "indexed": false},
                {"field_name": "age", "indexed": true}
            ]
        }
        "#;
        fs::write(config_dir.join("config.json"), config_content).unwrap();

        let fields = get_indexed_fields(data_path, entity, table);

        assert_eq!(fields, vec!["age", "id"]);
    }
}
