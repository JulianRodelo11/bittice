use std::path::Path;
use crate::core::storage::manifest::Manifest;
use crate::repl::state::{CatalogNode, CatalogNodeType};

pub fn get_catalog_tree() -> Vec<CatalogNode> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
    
    let mut nodes = Vec::new();
    if let Ok(entities) = std::fs::read_dir(data_path) {
        let mut entity_entries: Vec<_> = entities.flatten().collect();
        entity_entries.sort_by_key(|a| a.file_name());
        for entity in entity_entries {
            if let Ok(ft) = entity.file_type() {
                if ft.is_dir() {
                    let entity_name = entity.file_name().to_string_lossy().to_string();
                    let mut tables_nodes = Vec::new();
                    if let Ok(tables) = std::fs::read_dir(entity.path()) {
                        let mut table_entries: Vec<_> = tables.flatten().collect();
                        table_entries.sort_by_key(|a| a.file_name());
                        for table in table_entries {
                            if let Ok(t_ft) = table.file_type() {
                                if t_ft.is_dir() {
                                    let table_name = table.file_name().to_string_lossy().to_string();
                                    let mut fields = get_table_original_fields(&entity_name, &table_name);
                                    fields.sort();
                                    let field_nodes = fields.into_iter().map(|f| CatalogNode {
                                        name: f,
                                        node_type: CatalogNodeType::Field,
                                        children: Vec::new(),
                                        is_expanded: false,
                                        depth: 2,
                                    }).collect();
                                    
                                    tables_nodes.push(CatalogNode {
                                        name: table_name,
                                        node_type: CatalogNodeType::Table,
                                        children: field_nodes,
                                        is_expanded: true,
                                        depth: 1,
                                    });
                                }
                            }
                        }
                    }
                    nodes.push(CatalogNode {
                        name: entity_name,
                        node_type: CatalogNodeType::Entity,
                        children: tables_nodes,
                        is_expanded: true,
                        depth: 0,
                    });
                }
            }
        }
    }
    nodes
}

pub fn flatten_catalog_nodes(nodes: &[CatalogNode], result: &mut Vec<String>, parent_is_last: &[bool]) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        let mut line = String::new();
        
        match node.node_type {
            CatalogNodeType::Entity => {
                line.push_str(&format!("󰆼{}", node.name));
            }
            CatalogNodeType::Table => {
                let guide = if is_last { "└──" } else { "├──" };
                line.push_str(&format!("{}󰓫{}", guide, node.name));
            }
            CatalogNodeType::Field => {
                let mut guides = String::new();
                for &p_last in &parent_is_last[1..] {
                    if p_last { guides.push_str("   "); } else { guides.push_str("│  "); }
                }
                let branch = if is_last { "└──" } else { "├──" };
                line.push_str(&format!("{}{}󰇽{}", guides, branch, node.name));
            }
        }
        
        result.push(line);
        
        if node.is_expanded && !node.children.is_empty() {
            let mut next_parent_is_last = parent_is_last.to_vec();
            next_parent_is_last.push(is_last);
            flatten_catalog_nodes(&node.children, result, &next_parent_is_last);
        }
    }
}

pub fn toggle_catalog_node(nodes: &mut [CatalogNode], target_index: usize, current_index: &mut usize) -> bool {
    for node in nodes.iter_mut() {
        if *current_index == target_index {
            node.is_expanded = !node.is_expanded;
            return true;
        }
        *current_index += 1;
        
        if node.is_expanded && !node.children.is_empty() {
            if toggle_catalog_node(&mut node.children, target_index, current_index) {
                return true;
            }
        }
    }
    false
}

pub fn get_table_original_fields(entity: &str, table: &str) -> Vec<String> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
    let manifest_path = data_path.join(entity).join(table).join("manifest.json");
    if let Ok(file) = std::fs::File::open(manifest_path) {
        let reader = std::io::BufReader::new(file);
        if let Ok(manifest) = serde_json::from_reader::<_, Manifest>(reader) {
            return manifest.original_fields;
        }
    }
    Vec::new()
}

pub fn get_path_suggestions(input: &str) -> Vec<String> {
    let raw_query = if input.is_empty() { "./" } else { input };
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
    let (search_dir, prefix) = if query.ends_with(std::path::MAIN_SEPARATOR) {
        (path, "")
    } else if path.parent().is_none() {
        (Path::new("/"), path.to_str().unwrap_or(""))
    } else {
        (path.parent().unwrap(), path.file_name().and_then(|s| s.to_str()).unwrap_or(""))
    };
    let search_dir = if search_dir.as_os_str().is_empty() { Path::new("/") } else { search_dir };
    let mut suggestions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) && !name.starts_with('.') {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let is_ndjson = name.ends_with(".ndjson");
                if is_dir || is_ndjson {
                    let mut display_path = if raw_query.starts_with('~') {
                        let home = std::env::var("HOME").unwrap_or_default();
                        entry.path().to_string_lossy().replacen(&home, "~", 1)
                    } else {
                        entry.path().to_string_lossy().to_string()
                    };
                    if is_dir { display_path.push(std::path::MAIN_SEPARATOR); }
                    suggestions.push(display_path);
                }
            }
        }
    }
    suggestions.sort();
    suggestions
}

pub fn get_loaded_data() -> Vec<String> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
    
    let mut tree_lines = Vec::new();
    if !data_path.exists() {
        return vec!["Error: 'data' folder not found".to_string(), format!("Checked: {:?}", std::env::current_dir())];
    }

    if let Ok(entities) = std::fs::read_dir(data_path) {
        let mut entity_entries: Vec<_> = entities.flatten().collect();
        entity_entries.sort_by_key(|a| a.file_name());
        for entity in entity_entries {
            if let Ok(ft) = entity.file_type() {
                if ft.is_dir() {
                    let entity_name = entity.file_name().to_string_lossy().to_string();
                    let mut tables_list = Vec::new();
                    if let Ok(tables) = std::fs::read_dir(entity.path()) {
                        for table in tables.flatten() {
                            if let Ok(t_ft) = table.file_type() {
                                if t_ft.is_dir() {
                                    tables_list.push(table.file_name().to_string_lossy().to_string());
                                }
                            }
                        }
                    }
                    tables_list.sort();
                    if !tables_list.is_empty() {
                        tree_lines.push(format!("󰆼{}", entity_name)); // Database icon
                        for (i, table) in tables_list.iter().enumerate() {
                            let is_last_table = i == tables_list.len() - 1;
                            let prefix = if is_last_table { "└──" } else { "├──" };
                            tree_lines.push(format!("{}󰓫{}", prefix, table)); // Table icon
                            
                            // Original Fields
                            let fields = get_table_original_fields(&entity_name, table);
                            for (j, field) in fields.iter().enumerate() {
                                let is_last_field = j == fields.len() - 1;
                                let field_prefix = match (is_last_table, is_last_field) {
                                    (false, false) => "│  ├──",
                                    (false, true)  => "│  └──",
                                    (true, false)  => "   ├──",
                                    (true, true)   => "   └──",
                                };
                                tree_lines.push(format!("{}󰇽{}", field_prefix, field)); // Column icon
                            }
                        }
                    }
                }
            }
        }
    }
    tree_lines
}

pub fn get_indexed_fields(entity: &str, table: &str) -> Vec<String> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
    let table_path = data_path.join(entity).join(table);
    let mut all_fields = std::collections::HashSet::new();
    
    // Escaneamos TODOS los archivos .dat para tener la lista completa (Originales + Derivados)
    let segments_dir = table_path.join("segments");
    if let Ok(entries) = std::fs::read_dir(segments_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Ok(seg_files) = std::fs::read_dir(entry.path()) {
                    for f in seg_files.flatten() {
                        if let Some(name) = f.file_name().to_str() {
                            if name.ends_with(".dat") && !name.starts_with("bitmaps_") {
                                all_fields.insert(name.trim_end_matches(".dat").to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut result: Vec<String> = all_fields.into_iter().collect();
    result.sort();
    result
}

pub fn get_entities() -> Vec<String> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
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

pub fn get_field_values(entity: &str, table: &str, field: &str) -> Vec<String> {
    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() {
        data_path = Path::new("data");
    }
    let mut values = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
    let table_path = data_path.join(entity).join(table);
    let segments_dir = table_path.join("segments");
    if let Ok(entries) = std::fs::read_dir(segments_dir) {
        if let Some(seg_entry) = entries.filter_map(|e| e.ok()).find(|e| e.path().is_dir()) {
            let bitmap_path = seg_entry.path().join(format!("bitmaps_{}.dat", field));
            if bitmap_path.exists() {
                if let Ok(file) = std::fs::File::open(bitmap_path) {
                    if let Ok(bitmaps) = bincode::deserialize_from::<_, std::collections::HashMap<String, serde_json::Value>>(file) {
                        let mut keys: Vec<String> = bitmaps.keys().cloned().collect();
                        keys.sort();
                        for k in keys.into_iter().take(100) { if !k.is_empty() { values.push(k); } }
                    }
                }
            }
        }
    }
    values
}

pub fn get_order_by_fields(entity: &str, table: &str) -> Vec<String> {
    // Here we do return everything available for sorting
    let all = get_indexed_fields(entity, table);
    all
}

pub fn get_base_fields(all_fields: &[String]) -> Vec<String> {
    // THIS IS FOR THE "Fields" SECTION: We filter to leave only the original ones
    let mut filtered: Vec<String> = all_fields.iter()
        .filter(|f| {
            !f.ends_with("_day") && !f.ends_with("_month") && 
            !f.ends_with("_year") && !f.ends_with("_hour_bucket")
        })
        .cloned().collect();
    filtered.sort();
    filtered
}

pub fn get_filtered_fields(all_fields: &[String]) -> Vec<String> {
    // THIS IS FOR FILTERS/AGGS: We return everything
    let mut filtered = all_fields.to_vec();
    filtered.sort();
    filtered
}
