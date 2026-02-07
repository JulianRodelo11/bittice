use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use roaring::RoaringBitmap;
use crate::repl::state::Filter;

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn execute_query(
    entity: &str,
    table: &str,
    fields: &[String],
    filters: &[Filter],
    filters_op: &str,
    limit: usize,
) -> Result<QueryResult> {
    if fields.is_empty() {
        return Ok(QueryResult { headers: vec![], rows: vec![] });
    }

    let base_path = Path::new("data").join(entity).join(table);
    let index_dir = base_path.join("index");
    let stores_dir = base_path.join("stores");

    // 1. Determine target IDs based on filters
    let target_ids = if filters.is_empty() {
        // No filters: use all available IDs from the first field's bitmap if possible
        // or just rely on the fallback below.
        None
    } else {
        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;

        for f in filters {
            if f.field == "?" || f.value == "?" { continue; }
            
            let mut filter_bitmap = RoaringBitmap::new();
            let idx_path = index_dir.join(format!("{}.idx", f.field));
            
            if let Ok(file) = File::open(idx_path) {
                let reader = BufReader::new(file);
                for line in reader.lines().flatten() {
                    // Format: {field}__{val}\t{id}
                    if let Some(pos) = line.find("__") {
                        let rest = &line[pos + 2..];
                        if let Some(tab_pos) = rest.find('\t') {
                            let val = &rest[..tab_pos];
                            let id_str = &rest[tab_pos + 1..];
                            
                            // Simple equality for now (Eq)
                            if val == f.value {
                                if let Ok(id) = id_str.parse::<u32>() {
                                    filter_bitmap.insert(id);
                                }
                            }
                        }
                    }
                }
            }

            if first {
                result_bitmap = filter_bitmap;
                first = false;
            } else {
                if filters_op == "And" {
                    result_bitmap &= filter_bitmap;
                } else {
                    result_bitmap |= filter_bitmap;
                }
            }
        }
        Some(result_bitmap)
    };

    // 2. Fetch data for target IDs
    let mut data_map: HashMap<u32, HashMap<String, String>> = HashMap::new();
    
    // If we have no filters or filters yielded nothing
    let ids_to_fetch: Vec<u32> = if let Some(bitmap) = target_ids {
        bitmap.into_iter().take(limit).collect()
    } else {
        // Fallback: Read first N from any store file
        let mut fallback_ids = RoaringBitmap::new();
        for field in fields {
            let path = stores_dir.join(format!("{}.store", field));
            if let Ok(file) = File::open(&path) {
                for line in BufReader::new(file).lines().take(limit + 50) {
                    if let Ok(l) = line {
                        if let Some((id_s, _)) = l.split_once('\t') {
                            if let Ok(id) = id_s.parse::<u32>() {
                                fallback_ids.insert(id);
                                if fallback_ids.len() >= limit as u64 { break; }
                            }
                        }
                    }
                }
            }
            if !fallback_ids.is_empty() { break; }
        }
        fallback_ids.into_iter().take(limit).collect()
    };

    for field in fields {
        let path = stores_dir.join(format!("{}.store", field));
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            for line in reader.lines().flatten() {
                if let Some((id_s, val)) = line.split_once('\t') {
                    if let Ok(id) = id_s.parse::<u32>() {
                        if ids_to_fetch.contains(&id) {
                            data_map.entry(id).or_default().insert(field.clone(), val.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut sorted_ids = ids_to_fetch;
    sorted_ids.sort();

    let rows = sorted_ids.into_iter().map(|id| {
        let empty_map = HashMap::new();
        let row_data = data_map.get(&id).unwrap_or(&empty_map);
        fields.iter().map(|f| row_data.get(f).cloned().unwrap_or_default()).collect()
    }).collect();

    Ok(QueryResult { headers: fields.to_vec(), rows })
}
