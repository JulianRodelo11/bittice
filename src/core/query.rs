use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use roaring::RoaringBitmap;
use crate::repl::state::{Filter, ComparisonOp, LogicalOp, SortDirection};

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total_found: usize,
}

pub fn execute_query(
    entity: &str,
    table: &str,
    fields: &[String],
    filters: &[Filter],
    filters_op: &LogicalOp,
    aggregations: &[serde_json::Value],
    order_by: &[(String, SortDirection)],
    limit: usize,
    offset: usize,
) -> Result<QueryResult> {
    if fields.is_empty() && aggregations.is_empty() {
        return Ok(QueryResult { headers: vec![], rows: vec![], total_found: 0 });
    }

    let base_path = Path::new("data").join(entity).join(table);
    let index_dir = base_path.join("index");
    let stores_dir = base_path.join("stores");

    // 1. Determine target IDs based on filters
    let target_ids = if filters.is_empty() {
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
                    if let Some(pos) = line.find("__") {
                        let rest = &line[pos + 2..];
                        if let Some(tab_pos) = rest.find('\t') {
                            let val = &rest[..tab_pos];
                            let id_str = &rest[tab_pos + 1..];
                            
                            let matches = match f.op {
                                ComparisonOp::Eq => val == f.value,
                                ComparisonOp::In => f.value.split(',').any(|v| v.trim() == val),
                                ComparisonOp::Gte => val >= f.value.as_str(),
                                ComparisonOp::Lt => val < f.value.as_str(),
                                ComparisonOp::Ne => val != f.value,
                                ComparisonOp::Gt => val > f.value.as_str(),
                                ComparisonOp::Lte => val <= f.value.as_str(),
                                ComparisonOp::Like => {
                                    if f.value.starts_with('%') && f.value.ends_with('%') {
                                        val.contains(&f.value[1..f.value.len()-1])
                                    } else if f.value.ends_with('%') {
                                        val.starts_with(&f.value[..f.value.len()-1])
                                    } else if f.value.starts_with('%') {
                                        val.ends_with(&f.value[1..])
                                    } else {
                                        val == f.value
                                    }
                                }
                            };

                            if matches {
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
                match filters_op {
                    LogicalOp::And => result_bitmap &= filter_bitmap,
                    LogicalOp::Or => result_bitmap |= filter_bitmap,
                }
            }
        }
        Some(result_bitmap)
    };

    // 2. Fetch IDs to process
    let (ids_to_fetch_all, total_found): (Vec<u32>, usize) = if let Some(bitmap) = target_ids {
        let all: Vec<u32> = bitmap.into_iter().collect();
        let total = all.len();
        (all, total)
    } else {
        let mut fallback_ids = RoaringBitmap::new();
        let sample_field = fields.get(0).or_else(|| None);
        
        let path = if let Some(f) = sample_field {
            stores_dir.join(format!("{}.store", f))
        } else if let Ok(mut entries) = std::fs::read_dir(&stores_dir) {
             entries.find_map(|e| e.ok().map(|entry| entry.path())).unwrap_or_default()
        } else {
            Path::new("").to_path_buf()
        };

        let mut total = 0;
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines() {
                if let Ok(l) = line {
                    if let Some((id_s, _)) = l.split_once('\t') {
                        if let Ok(id) = id_s.parse::<u32>() {
                            fallback_ids.insert(id);
                            total += 1;
                        }
                    }
                }
                if total >= 10000 && !filters.is_empty() { break; } // Safety break
            }
        }
        (fallback_ids.into_iter().collect(), total)
    };

    // 3. Handle Aggregations if present
    if !aggregations.is_empty() {
        let mut res = handle_aggregations(&base_path, &ids_to_fetch_all, aggregations)?;
        res.total_found = res.rows.len();
        return Ok(res);
    }

    let mut final_ids = ids_to_fetch_all;

    // 4. Handle Sorting
    if !order_by.is_empty() {
        let mut sort_data: Vec<(u32, String)> = Vec::with_capacity(final_ids.len());
        let (sort_field, direction) = &order_by[0];
        
        let field_to_load = sort_field;

        let path = stores_dir.join(format!("{}.store", field_to_load));
        if let Ok(file) = File::open(path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Some((id_s, val)) = line.split_once('\t') {
                    if let Ok(id) = id_s.parse::<u32>() {
                        if final_ids.contains(&id) {
                            sort_data.push((id, val.to_string()));
                        }
                    }
                }
            }
        }
        
        sort_data.sort_by(|a, b| {
            let cmp = if let (Ok(na), Ok(nb)) = (a.1.parse::<f64>(), b.1.parse::<f64>()) {
                na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.1.cmp(&b.1)
            };
            
            let final_cmp = if cmp == std::cmp::Ordering::Equal {
                a.0.cmp(&b.0)
            } else {
                cmp
            };

            if *direction == SortDirection::Desc { 
                final_cmp.reverse() 
            } else { 
                final_cmp 
            }
        });
        
        final_ids = sort_data.into_iter().map(|(id, _)| id).skip(offset).take(limit).collect();
    } else {
        final_ids.sort();
        final_ids = final_ids.into_iter().skip(offset).take(limit).collect();
    }

    // 5. Fetch Final Data
    let mut data_map: HashMap<u32, HashMap<String, String>> = HashMap::new();
    for field in fields {
        let path = stores_dir.join(format!("{}.store", field));
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines().flatten() {
                if let Some((id_s, val)) = line.split_once('\t') {
                    if let Ok(id) = id_s.parse::<u32>() {
                        if final_ids.contains(&id) {
                            data_map.entry(id).or_default().insert(field.clone(), val.to_string());
                        }
                    }
                }
            }
        }
    }

    let rows = final_ids.into_iter().map(|id| {
        let empty_map = HashMap::new();
        let row_data = data_map.get(&id).unwrap_or(&empty_map);
        fields.iter().map(|f| row_data.get(f).cloned().unwrap_or_default()).collect()
    }).collect();

    Ok(QueryResult { headers: fields.to_vec(), rows, total_found })
}

fn handle_aggregations(base_path: &Path, ids: &[u32], aggregations: &[serde_json::Value]) -> Result<QueryResult> {
    let stores_dir = base_path.join("stores");
    let mut headers = Vec::new();
    let mut rows = Vec::new();

    for agg in aggregations {
        if let Some(obj) = agg.as_object() {
            let agg_type = obj.keys().next().unwrap();
            let params = obj.get(agg_type).unwrap();

            match agg_type.as_str() {
                "Count" => {
                    headers.push("count".to_string());
                    rows = vec![vec![ids.len().to_string()]];
                },
                "TopN" => {
                    let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                    
                    let mut counts = HashMap::new();
                    let path = stores_dir.join(format!("{}.store", field));
                    if let Ok(file) = File::open(path) {
                        for line in BufReader::new(file).lines().flatten() {
                            if let Some((id_s, val)) = line.split_once('\t') {
                                if let Ok(id) = id_s.parse::<u32>() {
                                    if ids.contains(&id) {
                                        *counts.entry(val.to_string()).or_insert(0) += 1;
                                    }
                                }
                            }
                        }
                    }
                    
                    let mut counts_vec: Vec<_> = counts.into_iter().collect();
                    counts_vec.sort_by(|a, b| b.1.cmp(&a.1));
                    
                    headers = vec![field.to_string(), "count".to_string()];
                    rows = counts_vec.into_iter().take(n)
                        .map(|(val, count)| vec![val, count.to_string()])
                        .collect();
                },
                "Sum" | "Avg" | "Min" | "Max" => {
                    let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                    let mut values = Vec::new();
                    let path = stores_dir.join(format!("{}.store", field));
                    if let Ok(file) = File::open(path) {
                        for line in BufReader::new(file).lines().flatten() {
                            if let Some((id_s, val)) = line.split_once('\t') {
                                if let Ok(id) = id_s.parse::<u32>() {
                                    if ids.contains(&id) {
                                        if let Ok(num) = val.parse::<f64>() {
                                            values.push(num);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    let result = match agg_type.as_str() {
                        "Sum" => values.iter().sum::<f64>(),
                        "Avg" => if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 },
                        "Min" => values.iter().copied().fold(f64::INFINITY, f64::min),
                        "Max" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                        _ => 0.0,
                    };
                    
                    headers.push(format!("{}_{}", agg_type.to_lowercase(), field));
                    if rows.is_empty() { rows.push(vec![result.to_string()]); }
                    else { rows[0].push(result.to_string()); }
                },
                "GroupBy" => {
                    let group_field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                    let operation = params.get("operation").and_then(|v| v.as_str()).unwrap_or("Count");
                    let value_field = params.get("value_field").and_then(|v| v.as_str());

                    let mut group_map: HashMap<String, Vec<f64>> = HashMap::new();
                    
                    // Load groups
                    let mut id_to_group = HashMap::new();
                    let path = stores_dir.join(format!("{}.store", group_field));
                    if let Ok(file) = File::open(path) {
                        for line in BufReader::new(file).lines().flatten() {
                            if let Some((id_s, val)) = line.split_once('\t') {
                                if let Ok(id) = id_s.parse::<u32>() {
                                    if ids.contains(&id) {
                                        id_to_group.insert(id, val.to_string());
                                    }
                                }
                            }
                        }
                    }

                    // Load values if needed
                    if let Some(vf) = value_field {
                        let path = stores_dir.join(format!("{}.store", vf));
                        if let Ok(file) = File::open(path) {
                            for line in BufReader::new(file).lines().flatten() {
                                if let Some((id_s, val)) = line.split_once('\t') {
                                    if let Ok(id) = id_s.parse::<u32>() {
                                        if let Some(group) = id_to_group.get(&id) {
                                            if let Ok(num) = val.parse::<f64>() {
                                                group_map.entry(group.clone()).or_default().push(num);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // Count doesn't need value_field
                        for group in id_to_group.values() {
                            group_map.entry(group.clone()).or_default().push(1.0);
                        }
                    }

                    headers = vec![group_field.to_string(), format!("{}_{}", operation.to_lowercase(), value_field.unwrap_or(""))];
                    rows = group_map.into_iter().map(|(group, vals)| {
                        let res = match operation {
                            "Sum" => vals.iter().sum::<f64>(),
                            "Avg" => if vals.is_empty() { 0.0 } else { vals.iter().sum::<f64>() / vals.len() as f64 },
                            "Min" => vals.iter().copied().fold(f64::INFINITY, f64::min),
                            "Max" => vals.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                            "Count" | _ => vals.len() as f64,
                        };
                        vec![group, res.to_string()]
                    }).collect();
                }
                _ => {}
            }
        }
    }

    Ok(QueryResult { headers, rows, total_found: 0 })
}
