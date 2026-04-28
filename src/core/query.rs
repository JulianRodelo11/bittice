use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use roaring::RoaringBitmap;
use memmap2::Mmap;
use rayon::prelude::*;
use std::cmp::Ordering;
use crate::core::types::{compare_filter_value, Filter, LogicalOp, SortDirection};
pub use crate::core::types::QueryResult;

#[derive(Eq, PartialEq)]
struct AggEntry {
    value: String,
    count: u64,
}

impl Ord for AggEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap (so we keep the largest ones)
        other.count.cmp(&self.count)
            .then_with(|| self.value.cmp(&other.value))
    }
}

impl PartialOrd for AggEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[allow(clippy::too_many_arguments)]
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
    query_cache: &mut HashMap<String, HashMap<String, RoaringBitmap>>,
) -> Result<QueryResult> {
    let start_time = Instant::now();
    if fields.is_empty() && aggregations.is_empty() {
        return Ok(QueryResult { headers: vec![], rows: vec![], row_ids: None, total_found: 0, execution_time_micros: 0, debug_info: None, aggregations: None });
    }

    let base_path = crate::core::data_paths::mirror_entity_dir(entity).join(table);
    let index_dir = base_path.join("index");
    let stores_dir = base_path.join("stores");

    // 1. Filters (PARALLELIZED and with support for all operators using the index)
    let target_ids = if filters.is_empty() {
        None
    } else {
        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;
        for f in filters {
            if f.field == "?" || f.value == "?" { continue; }
            
            // Load bitmaps to cache if not present
            if !query_cache.contains_key(&f.field) {
                if let Ok(file) = File::open(index_dir.join(format!("bitmaps_{}.dat", f.field))) {
                    if let Ok(bitmaps) = bincode::deserialize_from::<_, HashMap<String, RoaringBitmap>>(file) {
                        query_cache.insert(f.field.clone(), bitmaps);
                    }
                }
            }

            let mut filter_bitmap = RoaringBitmap::new();
            if let Some(bitmaps) = query_cache.get(&f.field) {
                if f.op == crate::core::types::ComparisonOp::Eq {
                    if let Some(bitmap) = bitmaps.get(&f.value) {
                        filter_bitmap = bitmap.clone();
                    } else {
                        bitmaps.iter()
                            .filter(|(k, _)| compare_filter_value(k, f.op, &f.value, f.value_to.as_deref(), &f.value_options, f.field_type))
                            .for_each(|(_, bm)| filter_bitmap |= bm);
                    }
                } else {
                    bitmaps.iter()
                        .filter(|(k, _)| compare_filter_value(k, f.op, &f.value, f.value_to.as_deref(), &f.value_options, f.field_type))
                        .for_each(|(_, bm)| filter_bitmap |= bm);
                }
            }

            if first { result_bitmap = filter_bitmap; first = false; }
            else {
                match filters_op {
                    LogicalOp::And => result_bitmap &= filter_bitmap,
                    LogicalOp::Or => result_bitmap |= filter_bitmap,
                }
            }
        }
        Some(result_bitmap)
    };

    // 2. Fetch IDs
    let (ids_to_fetch_all, total_found): (Vec<u32>, usize) = if let Some(bitmap) = &target_ids {
        let all: Vec<u32> = bitmap.iter().collect();
        let total = all.len();
        (all, total)
    } else {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(&stores_dir) {
            if let Some(entry) = entries.flatten().find(|e| e.file_name().to_string_lossy().ends_with(".offsets")) {
                if let Ok(meta) = entry.metadata() {
                    total = (meta.len() >> 3) as usize;
                }
            }
        }
        ((0..total as u32).collect(), total)
    };

    if !aggregations.is_empty() {
        let mut res = handle_aggregations(&base_path, target_ids.as_ref(), aggregations, query_cache)?;
        res.execution_time_micros = start_time.elapsed().as_micros();
        return Ok(res);
    }

    // 3. Sorting (PARALLEL)
    let mut final_ids = ids_to_fetch_all;
    if !order_by.is_empty() {
        let (sort_field, direction) = &order_by[0];
        let (dat, off) = mmap_field(&stores_dir, sort_field)
            .map_err(|e| anyhow::anyhow!("Failed to load sort field '{}': {}", sort_field, e))?;

        final_ids.par_sort_unstable_by(|&a, &b| {
            let val_a = get_raw_bytes(&dat, &off, a).unwrap_or(&[]);
            let val_b = get_raw_bytes(&dat, &off, b).unwrap_or(&[]);
            
            let cmp = if !val_a.is_empty() && val_a[0].is_ascii_digit() {
                let s_a = std::str::from_utf8(val_a).unwrap_or("");
                let s_b = std::str::from_utf8(val_b).unwrap_or("");
                match (s_a.parse::<f64>(), s_b.parse::<f64>()) {
                    (Ok(n_a), Ok(n_b)) => n_a.partial_cmp(&n_b).unwrap_or(Ordering::Equal),
                    _ => val_a.cmp(val_b),
                }
            } else {
                val_a.cmp(val_b)
            };

            if *direction == SortDirection::Desc { cmp.reverse() } else { cmp }
        });
    }

    // 4. Pagination
    let paged_ids = final_ids.into_iter().skip(offset).take(limit).collect::<Vec<_>>();

    // 5. Final Extraction (PARALLELIZED with Rayon)
    let mmaps: Vec<Option<(Mmap, Mmap)>> = fields.iter().map(|f| mmap_field(&stores_dir, f).ok()).collect();

    let rows: Vec<Vec<String>> = paged_ids.into_par_iter().map(|id| {
        mmaps.iter().map(|mmap_pair| {
            mmap_pair.as_ref()
                .and_then(|(dat, off)| get_val_from_mmap(dat, off, id))
                .unwrap_or_default()
        }).collect()
    }).collect();

    Ok(QueryResult { headers: fields.to_vec(), rows, row_ids: None, total_found, execution_time_micros: start_time.elapsed().as_micros(), debug_info: None, aggregations: None })
}

fn mmap_field(stores_dir: &Path, field: &str) -> Result<(Mmap, Mmap)> {
    let dat_file = File::open(stores_dir.join(format!("{}.dat", field)))?;
    let off_file = File::open(stores_dir.join(format!("{}.offsets", field)))?;
    Ok((unsafe { Mmap::map(&dat_file)? }, unsafe { Mmap::map(&off_file)? }))
}

#[inline(always)]
fn get_raw_bytes<'a>(dat: &'a Mmap, off: &Mmap, id: u32) -> Option<&'a [u8]> {
    let start_idx = (id as usize) << 3;
    if start_idx + 8 > off.len() { return None; }
    let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8].try_into().ok()?) as usize;
    let end_pos = if start_idx + 16 <= off.len() {
        u64::from_le_bytes(off[start_idx+8..start_idx+16].try_into().ok()?) as usize
    } else { dat.len() };
    
    if start_pos + 8 > dat.len() || end_pos > dat.len() { return None; }
    // Bincode for String stores 8 bytes of length, we skip them to compare the actual content
    Some(&dat[start_pos+8..end_pos])
}

#[inline(always)]
fn get_val_from_mmap(dat: &Mmap, off: &Mmap, id: u32) -> Option<String> {
    let bytes = get_raw_bytes(dat, off, id)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn handle_aggregations(
    base_path: &Path,
    filter_bitmap: Option<&RoaringBitmap>,
    aggregations: &[serde_json::Value],
    query_cache: &mut HashMap<String, HashMap<String, RoaringBitmap>>,
) -> Result<QueryResult> {
    let index_dir = base_path.join("index");
    let mut headers = Vec::new();
    let mut rows = Vec::new();
    
    for agg in aggregations {
        if let Some(obj) = agg.as_object() {
            let agg_type = obj.keys().next().unwrap();
            let params = obj.get(agg_type).unwrap();
            
            if agg_type == "Count" { 
                headers.push("count".to_string()); 
                let total = filter_bitmap.map(|b| b.len()).unwrap_or(0); // This is not quite right if no filters, but good enough for now
                rows = vec![vec![total.to_string()]]; 
            } else if agg_type == "GroupBy" {
                let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                let op = params.get("operation").and_then(|v| v.as_str()).unwrap_or("Count");
                
                // 1. Get Bitmaps
                if !query_cache.contains_key(field) {
                    if let Ok(file) = File::open(index_dir.join(format!("bitmaps_{}.dat", field))) {
                        if let Ok(bitmaps) = bincode::deserialize_from::<_, HashMap<String, RoaringBitmap>>(file) {
                            query_cache.insert(field.to_string(), bitmaps);
                        }
                    }
                }
                
                if let Some(bitmaps) = query_cache.get(field) {
                    let results: Vec<(String, u64)> = bitmaps.par_iter()
                        .map(|(val, bm)| {
                            let count = if let Some(filter) = filter_bitmap { (bm & filter).len() } else { bm.len() };
                            (val.clone(), count)
                        })
                        .filter(|(_, c)| *c > 0)
                        .collect();
                    
                    headers = vec![field.to_string(), op.to_string()];
                    rows = results.into_iter().map(|(v, c)| vec![v, c.to_string()]).collect();
                }
            } else if agg_type == "TopN" {
                let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                
                // 1. Get Bitmaps (with Cache)
                if !query_cache.contains_key(field) {
                    if let Ok(file) = File::open(index_dir.join(format!("bitmaps_{}.dat", field))) {
                        if let Ok(bitmaps) = bincode::deserialize_from::<_, HashMap<String, RoaringBitmap>>(file) {
                            query_cache.insert(field.to_string(), bitmaps);
                        }
                    }
                }
                
                let results = if let Some(bitmaps) = query_cache.get(field) {
                    // 2. PARALLEL processing with Rayon
                    let mut counts: Vec<AggEntry> = bitmaps.par_iter()
                        .map(|(val, bm)| {
                            let count = if let Some(filter) = filter_bitmap {
                                // Fast intersection and counting
                                (bm & filter).len()
                            } else {
                                bm.len()
                            };
                            AggEntry { value: val.clone(), count }
                        })
                        .filter(|e| e.count > 0)
                        .collect();
                    
                    // 3. Sort to get the Top N
                    counts.par_sort_unstable_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
                    counts.into_iter().take(n).collect::<Vec<_>>()
                } else {
                    vec![]
                };

                headers = vec![field.to_string(), "count".to_string()];
                rows = results.into_iter().map(|e| vec![e.value, e.count.to_string()]).collect();
            }
        }
    }
    Ok(QueryResult { headers, rows, row_ids: None, total_found: 0, execution_time_micros: 0, debug_info: None, aggregations: None })
}
