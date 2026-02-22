use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, RwLock};
use memmap2::Mmap;
use anyhow::{Result, Context};
use roaring::RoaringBitmap;
use crate::core::storage::manifest::Manifest;
use crate::core::storage::segment::{Segment, SegmentWriter};
use crate::core::storage::wal::{Wal, WalOperation};
use crate::core::types::{Filter, LogicalOp, OrderBy, QueryResult, SortDirection};
use rayon::prelude::*;
use std::cmp::Ordering;

pub struct Table {
    pub name: String,
    pub base_path: PathBuf,
    pub manifest: Manifest,
    active_segment: Option<SegmentWriter>,
    immutable_segments: Vec<Segment>,
    wal: Wal,
    index_cache: Arc<RwLock<HashMap<(u64, String), Arc<HashMap<String, RoaringBitmap>>>>>,
    /// External ID -> (Segment ID, Local ID)
    pub primary_index: HashMap<String, (u64, u32)>,
}

// Estructuras para el Heap
#[derive(PartialEq)]
struct HeapItem {
    key: SortKey,
    seg_id: u64,
    local_id: u32,
}

#[derive(PartialEq, PartialOrd, Clone, Debug)]
enum SortKey {
    Num(f64),
    Str(String),
    None
}

enum RefSortKey<'a> {
    Num(f64),
    Str(&'a str),
    None
}

fn compare_ref_owned(r: &RefSortKey, o: &SortKey) -> Ordering {
    match (r, o) {
        (RefSortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (RefSortKey::Str(a), SortKey::Str(b)) => (*a).cmp(b.as_str()),
        (RefSortKey::None, SortKey::None) => Ordering::Equal,
        // Discriminant order matches SortKey: Num(0), Str(1), None(2)
        (RefSortKey::Num(_), _) => Ordering::Less,
        (RefSortKey::Str(_), SortKey::Num(_)) => Ordering::Greater,
        (RefSortKey::Str(_), SortKey::None) => Ordering::Less,
        (RefSortKey::None, _) => Ordering::Greater,
    }
}

impl Eq for SortKey {}
impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl Table {
    pub fn open(base_path: &Path, name: &str) -> Result<Self> {
        let table_path = base_path.join(name);
        if !table_path.exists() {
            fs::create_dir_all(&table_path).context("Failed to create table directory")?;
        }

        let segments_dir = table_path.join("segments");
        if !segments_dir.exists() {
            fs::create_dir_all(&segments_dir).context("Failed to create segments directory")?;
        }

        let manifest_path = table_path.join("manifest.json");
        let manifest: Manifest = if manifest_path.exists() {
            let file = fs::File::open(&manifest_path)?;
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader)?
        } else {
            Manifest::new()
        };

        let wal_path = table_path.join("wal.log");
        let wal = Wal::open(&wal_path)?;
        
        let mut table = Table {
            name: name.to_string(),
            base_path: table_path,
            manifest,
            active_segment: None,
            immutable_segments: Vec::new(),
            wal,
            index_cache: Arc::new(RwLock::new(HashMap::new())),
            primary_index: HashMap::new(),
        };

        table.load_segments()?;
        table.ensure_active_segment()?;
        table.load_primary_index()?;

        Ok(table)
    }

    fn load_primary_index(&mut self) -> Result<()> {
        let idx_path = self.base_path.join("primary.idx");
        if idx_path.exists() {
            let file = fs::File::open(idx_path)?;
            let reader = std::io::BufReader::new(file);
            self.primary_index = bincode::deserialize_from(reader).unwrap_or_default();
        }
        Ok(())
    }

    fn save_primary_index(&self) -> Result<()> {
        let idx_path = self.base_path.join("primary.idx");
        let file = fs::File::create(idx_path)?;
        let writer = std::io::BufWriter::new(file);
        bincode::serialize_into(writer, &self.primary_index)?;
        Ok(())
    }

    fn load_segments(&mut self) -> Result<()> {
        let segments_dir = self.base_path.join("segments");
        let active_id = self.manifest.active_segment_id;

        // Parallel load of immutable segments using Rayon
        self.immutable_segments = self.manifest.segments.par_iter()
            .filter(|seg_meta| seg_meta.id != active_id)
            .filter_map(|seg_meta| {
                let seg_path = segments_dir.join(format!("seg_{:04}", seg_meta.id));
                if seg_path.exists() {
                    Segment::load(&seg_path, Some(seg_meta)).ok()
                } else {
                    None
                }
            })
            .collect();

        // Sort by ID to maintain order
        self.immutable_segments.sort_by_key(|s| s.id);
        
        Ok(())
    }

    fn ensure_active_segment(&mut self) -> Result<()> {
        if self.active_segment.is_some() {
            return Ok(());
        }

        let segments_dir = self.base_path.join("segments");
        let active_id = self.manifest.active_segment_id;
        
        let seg_path = segments_dir.join(format!("seg_{:04}", active_id));
        let segment = if seg_path.exists() {
             let meta = self.manifest.segments.iter().find(|s| s.id == active_id);
             let mut s = Segment::load(&seg_path, meta)?;
             s.is_immutable = false;
             s
        } else {
            let s = Segment::new(active_id, &segments_dir);
            s.create_dirs()?;
            s
        };

        self.active_segment = Some(SegmentWriter::new(segment));
        Ok(())
    }

    pub fn insert(&mut self, row_data: HashMap<String, String>) -> Result<()> {
        let row_bytes = serde_json::to_vec(&row_data)?;
        let pk_field = if self.manifest.primary_key.is_empty() { "PK" } else { &self.manifest.primary_key };
        let id = row_data.get(pk_field).cloned().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        
        let op = WalOperation::Insert { id: id.clone(), data: row_bytes };
        self.wal.append(&op)?;

        if let Some(writer) = &mut self.active_segment {
             let local_id = writer.segment.record_count as u32;
             writer.append_record(&row_data)?; 
             self.primary_index.insert(id, (writer.segment.id, local_id));
        }

        Ok(())
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        if let Some((seg_id, local_id)) = self.primary_index.remove(id) {
            let op = WalOperation::Delete { id: id.to_string() };
            self.wal.append(&op)?;

            // Mark as deleted in segment
            if let Some(writer) = &mut self.active_segment {
                if writer.segment.id == seg_id {
                    writer.segment.mark_deleted(local_id)?;
                } else if let Some(seg) = self.immutable_segments.iter_mut().find(|s| s.id == seg_id) {
                    seg.mark_deleted(local_id)?;
                }
            }
        }
        Ok(())
    }

    pub fn update(&mut self, id: &str, row_data: HashMap<String, String>) -> Result<()> {
        self.delete(id)?;
        self.insert(row_data)?;
        Ok(())
    }

    pub fn set_original_fields(&mut self, fields: Vec<String>) -> Result<()> {
        self.manifest.original_fields = fields;
        self.save_manifest()
    }

    fn save_manifest(&self) -> Result<()> {
        let manifest_path = self.base_path.join("manifest.json");
        let temp_path = self.base_path.join("manifest.tmp");
        {
            let file = fs::File::create(&temp_path)?;
            serde_json::to_writer_pretty(&file, &self.manifest)?;
            file.sync_all()?;
        }
        fs::rename(&temp_path, &manifest_path)?;
        Ok(())
    }

    pub fn flush_active_segment(&mut self) -> Result<()> {
        if let Some(mut writer) = self.active_segment.take() {
            writer.flush()?;
            let meta = writer.segment.to_meta();
            self.manifest.add_segment(meta);
            self.manifest.active_segment_id += 1;
            self.manifest.last_sequence_number += 1; 
            
            self.save_manifest()?;
            self.save_primary_index()?;
            self.wal.truncate()?;
            
            let mut immutable_seg = writer.segment;
            immutable_seg.is_immutable = true;
            self.immutable_segments.push(immutable_seg);
        }
        self.ensure_active_segment()?;
        Ok(())
    }

    pub fn search(
        &self,
        fields: &[String],
        filters: &[Filter],
        filters_op: &LogicalOp,
        aggregations: &[serde_json::Value],
        order_by: &[OrderBy],
        limit: usize,
        offset: usize
    ) -> Result<QueryResult> {
        let start_time = std::time::Instant::now();
        
        let mut segment_tasks: Vec<&Segment> = self.immutable_segments.iter().collect();
        if let Some(writer) = &self.active_segment {
            segment_tasks.push(&writer.segment);
        }

        let filter_start = std::time::Instant::now();
        let cache_ref = &self.index_cache;

        // --- OPTIMIZATION: Primary Key Lookup ---
        let pk_field = if self.manifest.primary_key.is_empty() { "PK" } else { &self.manifest.primary_key };
        let pk_filter = filters.iter().find(|f| f.field == pk_field && f.op == crate::core::types::ComparisonOp::Eq);
        
        let segment_matches = if let Some(f) = pk_filter {
            // If we have a PK filter and it's an AND operation (or single filter), we can use the index directly.
            let use_index = filters.len() == 1 || *filters_op == LogicalOp::And;
            
            if use_index {
                if let Some((seg_id, local_id)) = self.primary_index.get(&f.value) {
                    let mut bm = RoaringBitmap::new();
                    bm.insert(*local_id);
                    vec![(*seg_id, bm)]
                } else {
                    vec![] // PK not found, result is empty
                }
            } else {
                self.scan_segments_parallel(&segment_tasks, filters, filters_op, &cache_ref)?
            }
        } else {
            self.scan_segments_parallel(&segment_tasks, filters, filters_op, &cache_ref)?
        };

        let mut total_found = 0;
        for (_, bitmap) in &segment_matches {
            total_found += bitmap.len();
        }
        
        let filter_elapsed = filter_start.elapsed().as_micros();

        let mut aggregation_results = Vec::new();
        let agg_start = std::time::Instant::now();
        if !aggregations.is_empty() {
            for agg in aggregations {
                // ... existing aggregation logic ...
                let mut agg_headers = Vec::new();
                let mut agg_rows = Vec::new();
                if let Some(obj) = agg.as_object() {
                    let agg_type = obj.keys().next().unwrap();
                    let params = obj.get(agg_type).unwrap();
                    if agg_type == "Count" {
                        // Global Count: Only summary
                        aggregation_results.push(crate::core::types::AggregationResult { 
                            headers: vec![], 
                            rows: vec![], 
                            summary: Some(total_found as f64) 
                        });
                        continue;
                    } else if agg_type == "GroupBy" || agg_type == "TopN" {
                        let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                        let mut global_counts: HashMap<String, u64> = HashMap::new();
                        for (seg_id, bitmap) in &segment_matches {
                            let seg_counts = if let Some(writer) = &self.active_segment {
                                if writer.segment.id == *seg_id { writer.get_counts(field, bitmap)? }
                                else { 
                                    let s = self.immutable_segments.iter().find(|s| s.id == *seg_id).unwrap();
                                    s.get_counts_thread_safe(field, bitmap, &cache_ref)? 
                                }
                            } else { 
                                let s = self.immutable_segments.iter().find(|s| s.id == *seg_id).unwrap();
                                s.get_counts_thread_safe(field, bitmap, &cache_ref)? 
                            };
                            for (val, count) in seg_counts { *global_counts.entry(val).or_insert(0) += count; }
                        }
                        
                        let mut results: Vec<(String, u64)> = global_counts.into_iter().collect();
                        if agg_type == "TopN" {
                            let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                            results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                            results.truncate(n);
                        } else {
                            let direction = order_by.iter().find(|o| o.field == field).map(|o| o.direction).unwrap_or(SortDirection::Asc);
                            results.sort_by(|a, b| {
                                let cmp = a.0.cmp(&b.0);
                                if direction == SortDirection::Desc { cmp.reverse() } else { cmp }
                            });
                        }

                        // Calcular el summary: suma de los conteos que REALMENTE se van a devolver
                        let total_count: u64 = results.iter().map(|(_, c)| c).sum();
                        
                        agg_headers = vec![field.to_string(), "count".to_string()];
                        agg_rows = results.into_iter().map(|(v, c)| vec![v, c.to_string()]).collect();
                        
                        aggregation_results.push(crate::core::types::AggregationResult { 
                            headers: agg_headers, 
                            rows: agg_rows, 
                            summary: Some(total_count as f64) 
                        });
                        continue;
                    } else if agg_type == "Sum" {
                        let field_name_opt = params.get("field").and_then(|v| v.as_str()).filter(|&s| s != "?");
                        let expression_str = params.get("expression").and_then(|v| v.as_str()).unwrap_or("0");
                        
                        if let Ok(expr) = crate::core::expression::parse_expression(expression_str) {
                            let required_fields = crate::core::expression::extract_fields(&expr);
                            
                            if let Some(field_name) = field_name_opt {
                                // --- Grouped Sum ---
                                // Parallel processing only if we have enough rows
                                let use_par = total_found > 500;
                                
                                let segment_group_results: Vec<HashMap<String, f64>> = if use_par {
                                    segment_matches.par_iter()
                                        .map(|(seg_id, bitmap)| self.process_seg_sum_grouped(*seg_id, bitmap, field_name, &required_fields, &expr))
                                        .collect()
                                } else {
                                    segment_matches.iter()
                                        .map(|(seg_id, bitmap)| self.process_seg_sum_grouped(*seg_id, bitmap, field_name, &required_fields, &expr))
                                        .collect()
                                };

                                let mut global_group_sums = HashMap::new();
                                let mut total_sum = 0.0;
                                for seg_map in segment_group_results {
                                    for (k, v) in seg_map {
                                        total_sum += v;
                                        *global_group_sums.entry(k).or_insert(0.0) += v;
                                    }
                                }

                                let mut results: Vec<(String, f64)> = global_group_sums.into_iter().collect();
                                results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
                                
                                agg_headers = vec![field_name.to_string(), "sum".to_string()];
                                agg_rows = results.into_iter().map(|(v, s)| vec![v, format!("{:.2}", s)]).collect();
                                aggregation_results.push(crate::core::types::AggregationResult { 
                                    headers: agg_headers, 
                                    rows: agg_rows, 
                                    summary: Some(total_sum) 
                                });
                                continue; 
                            } else {
                                // --- Global Sum ---
                                let use_par = total_found > 500;
                                let total_sum: f64 = if use_par {
                                    segment_matches.par_iter()
                                        .map(|(seg_id, bitmap)| self.process_seg_sum_global(*seg_id, bitmap, &required_fields, &expr))
                                        .sum()
                                } else {
                                    segment_matches.iter()
                                        .map(|(seg_id, bitmap)| self.process_seg_sum_global(*seg_id, bitmap, &required_fields, &expr))
                                        .sum()
                                };
                                
                                aggregation_results.push(crate::core::types::AggregationResult { 
                                    headers: vec![], 
                                    rows: vec![], 
                                    summary: Some(total_sum) 
                                });
                                continue;
                            }
                        } else {
                             agg_headers = vec!["error".to_string()];
                             agg_rows = vec![vec!["Invalid expression".to_string()]];
                        }
                    }
                }
                aggregation_results.push(crate::core::types::AggregationResult { headers: agg_headers, rows: agg_rows, summary: None });
            }
            
            if fields.is_empty() {
                let total_elapsed = start_time.elapsed().as_micros();
                let agg_elapsed = agg_start.elapsed().as_micros();
                let debug = format!("Filter: {}ms, Agg: {}ms, Segments: {}", 
                    filter_elapsed / 1000, agg_elapsed / 1000, segment_tasks.len());

                return Ok(QueryResult { 
                    headers: vec![], 
                    rows: vec![], 
                    total_found: total_found as usize, 
                    execution_time_micros: total_elapsed, 
                    debug_info: Some(debug),
                    aggregations: Some(aggregation_results)
                });
            }
        }

        // Si el límite es 0, no necesitamos procesar filas ni ordenamiento
        if limit == 0 {
            let total_elapsed = start_time.elapsed().as_micros();
            let debug = format!("Filter: {}ms, Limit: 0, Segments: {}", 
                filter_elapsed / 1000, segment_tasks.len());

            return Ok(QueryResult { 
                headers: fields.to_vec(), 
                rows: vec![], 
                total_found: total_found as usize, 
                execution_time_micros: total_elapsed, 
                debug_info: Some(debug),
                aggregations: if aggregation_results.is_empty() { None } else { Some(aggregation_results) }
            });
        }

        // --- ORDENAMIENTO ---
        let sort_start = std::time::Instant::now();
        let mut final_ids: Vec<(u64, u32)>;
        if !order_by.is_empty() {
            let sort_field = &order_by[0].field;
            let direction = order_by[0].direction;
            let limit_n = if limit == 0 { 100 } else { limit };
            let effective_limit = offset + limit_n; 

            // 1. Recolectar todos los IDs para procesar en paralelo
            let mut all_ids = Vec::with_capacity(total_found as usize);
            for (seg_id, bitmap) in &segment_matches {
                for id in bitmap {
                    all_ids.push((*seg_id, id));
                }
            }

            // 2. Preparar MMAPs para acceso thread-safe
            let mut mmap_refs = HashMap::new();
            for (seg_id, _) in &segment_matches {
                 let segment = if let Some(writer) = &self.active_segment {
                    if writer.segment.id == *seg_id { Some(&writer.segment) }
                    else { self.immutable_segments.iter().find(|s| s.id == *seg_id) }
                } else {
                    self.immutable_segments.iter().find(|s| s.id == *seg_id)
                };
                if let Some(s) = segment {
                    if let Ok(m) = s.get_mmap_pair(sort_field) {
                        mmap_refs.insert(*seg_id, m);
                    }
                }
            }

            // 3. Parallel Top-K
            let top_k_items = all_ids.par_chunks(4096)
                // Accumulator: (Vec<HeapItem>, Option<SortKey>) - Vec for items, Option for threshold
                .fold(
                || (Vec::with_capacity(effective_limit * 4), None::<SortKey>),
                |mut state, chunk| {
                    let acc = &mut state.0;
                    let threshold = &mut state.1;
                    let buffer_limit = effective_limit * 4;

                    for (seg_id, local_id) in chunk {
                        let ref_key = if let Some(mmap_pair) = mmap_refs.get(seg_id) {
                            let dat = &mmap_pair.0;
                            let off = &mmap_pair.1;
                            let start_idx = (*local_id as usize) << 3;
                            if start_idx + 8 <= off.len() {
                                let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8].try_into().unwrap()) as usize;
                                if start_pos + 8 <= dat.len() {
                                    let len = u64::from_le_bytes(dat[start_pos..start_pos+8].try_into().unwrap()) as usize;
                                    if let Ok(s) = std::str::from_utf8(&dat[start_pos+8..start_pos+8+len]) {
                                        if !s.is_empty() && s.as_bytes()[0].is_ascii_digit() {
                                            if let Ok(n) = s.parse::<f64>() { RefSortKey::Num(n) } 
                                            else { RefSortKey::Str(s) }
                                        } else { RefSortKey::Str(s) }
                                    } else { RefSortKey::None }
                                } else { RefSortKey::None }
                            } else { RefSortKey::None }
                        } else { RefSortKey::None };

                        // Check against threshold BEFORE allocation
                        if let Some(thresh) = threshold {
                             let cmp = compare_ref_owned(&ref_key, thresh);
                             let skip = if direction == SortDirection::Desc {
                                 cmp != Ordering::Greater // We want Greater. Skip if Less or Equal.
                             } else {
                                 cmp != Ordering::Less // We want Less. Skip if Greater or Equal.
                             };
                             if skip { continue; }
                        }

                        // Allocate and Push
                        let key = match ref_key {
                            RefSortKey::Num(n) => SortKey::Num(n),
                            RefSortKey::Str(s) => SortKey::Str(s.to_string()),
                            RefSortKey::None => SortKey::None,
                        };
                        acc.push(HeapItem { key, seg_id: *seg_id, local_id: *local_id });

                        // Compact if full
                        if acc.len() >= buffer_limit {
                             acc.sort_unstable_by(|a, b| {
                                let cmp = a.cmp(b);
                                if direction == SortDirection::Desc { cmp.reverse() } else { cmp }
                             });
                             if acc.len() > effective_limit {
                                 acc.truncate(effective_limit);
                                 // Update threshold
                                 if let Some(last) = acc.last() {
                                     *threshold = Some(last.key.clone());
                                 }
                             }
                        }
                    }
                    state
                }
            )
            .map(|(acc, _)| acc)
            .reduce(
                || Vec::with_capacity(effective_limit),
                |mut a, b| {
                    a.extend(b);
                    a.sort_unstable_by(|x, y| {
                         let cmp = x.cmp(y);
                         if direction == SortDirection::Desc { cmp.reverse() } else { cmp }
                    });
                    if a.len() > effective_limit {
                        a.truncate(effective_limit);
                    }
                    a
                }
            );

            final_ids = top_k_items.into_iter().skip(offset).map(|item| (item.seg_id, item.local_id)).collect();
        } else {
            final_ids = Vec::with_capacity(limit);
            let mut skipped = 0;
            'collect: for (seg_id, bitmap) in &segment_matches {
                for id in bitmap {
                    if skipped < offset { skipped += 1; continue; }
                    final_ids.push((*seg_id, id));
                    if final_ids.len() >= limit { break 'collect; }
                }
            }
        }
        let sort_elapsed = sort_start.elapsed().as_micros();

        // --- FETCHING ---
        let fetch_start = std::time::Instant::now();
        let mut segments_map = HashMap::new();
        for s in &self.immutable_segments {
            segments_map.insert(s.id, s);
        }
        if let Some(writer) = &self.active_segment {
            segments_map.insert(writer.segment.id, &writer.segment);
        }

        // Optimization: Identify unique segments actually needed for the result set
        // With LIMIT 100, we likely only need 1 or 2 segments, not all of them.
        let needed_seg_ids: std::collections::HashSet<u64> = final_ids.iter().map(|(id, _)| *id).collect();

        // 1. Pre-resolve all MMAPs for all involved segments and fields (PARALLEL)
        let segment_field_mmaps: HashMap<u64, Vec<Option<Arc<(Mmap, Mmap)>>>> = segments_map.par_iter()
            .filter(|(id, _)| needed_seg_ids.contains(id))
            .map(|(id, segment)| {
                // Optimization: Batch open all fields to reduce lock contention
                // This is especially critical when opening hundreds of files for a wide query (Cold Start)
                // Ignore errors here, as individual get_mmap_pair calls will fail if needed later
                let _ = segment.ensure_mmaps_batch(fields);

                let mmaps = segment.get_mmaps_batch(fields);
                (*id, mmaps)
            })
            .collect();

        // 2. Optimized Parallel Fetching (Vectorized)
        let use_vectorized = total_found > 100 || fields.len() >= 4;
        let fetch_mode_str = if use_vectorized { "Vectorized" } else { "Row" };
        let mut chunks_count = 0;

        let rows: Vec<Vec<String>> = if use_vectorized {
            const CHUNK_SIZE: usize = 512;
            chunks_count = (final_ids.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

            final_ids.par_chunks(CHUNK_SIZE)
                .flat_map(|chunk| {
                    // 1. Columnar Read: Read full columns into temporary buffers
                    // Use parallel iteration over fields to maximize throughput, 
                    // especially when chunks_count is small (e.g. 1 chunk but 100+ fields)
                    let columns_data: Vec<Vec<String>> = fields.par_iter().enumerate()
                        .map(|(field_idx, _)| {
                            let mut col_values = Vec::with_capacity(chunk.len());
                            
                            // Optimization: Cache last segment's mmap to avoid map lookup per row
                            let mut last_seg_id = u64::MAX;
                            let mut last_mmap: Option<&(Mmap, Mmap)> = None;

                            for (seg_id, local_id) in chunk {
                                if *seg_id != last_seg_id {
                                    last_seg_id = *seg_id;
                                    last_mmap = None;
                                    if let Some(mmaps) = segment_field_mmaps.get(seg_id) {
                                        if let Some(mmap_pair) = &mmaps[field_idx] {
                                            last_mmap = Some(&**mmap_pair);
                                        }
                                    }
                                }

                                let val = if let Some(mmap_pair) = last_mmap {
                                    unsafe { Segment::read_value_unchecked(mmap_pair, *local_id) }
                                } else {
                                    String::new()
                                };
                                col_values.push(val);
                            }
                            col_values
                        })
                        .collect();

                    // 2. Transpose: Construct rows from columns
                    // This is a fast memory-only operation
                    let mut chunk_rows = Vec::with_capacity(chunk.len());
                    let mut col_iters: Vec<_> = columns_data.into_iter().map(|c| c.into_iter()).collect();

                    for _ in 0..chunk.len() {
                        let mut row = Vec::with_capacity(fields.len());
                        for iter in &mut col_iters {
                            // Safe unwrap: all columns have exactly chunk.len() elements
                            row.push(iter.next().unwrap());
                        }
                        chunk_rows.push(row);
                    }
                    
                    chunk_rows
                })
                .collect()
        } else {
            final_ids.par_iter()
                .map(|(seg_id, local_id)| {
                    if let Some(segment) = segments_map.get(seg_id) {
                        let mmaps = segment_field_mmaps.get(seg_id).unwrap();
                        segment.get_row_values_from_mmaps(*local_id, mmaps)
                    } else {
                        vec![String::new(); fields.len()]
                    }
                })
                .collect()
        };
        
        let fetch_elapsed = fetch_start.elapsed().as_micros();

        let total_elapsed = start_time.elapsed().as_micros();
        let debug = format!("Filter: {}ms, Sort: {}ms, Fetch: {}ms, Segments: {}, FetchMode: {}, Chunks: {}", 
            filter_elapsed / 1000, sort_elapsed / 1000, fetch_elapsed / 1000, segment_tasks.len(), fetch_mode_str, chunks_count);
        
        Ok(QueryResult { 
            headers: fields.to_vec(), 
            rows, 
            total_found: total_found as usize, 
            execution_time_micros: total_elapsed, 
            debug_info: Some(debug),
            aggregations: if aggregation_results.is_empty() { None } else { Some(aggregation_results) }
        })
    }

    fn scan_segments_parallel(
        &self,
        segment_tasks: &[&Segment],
        filters: &[Filter],
        filters_op: &LogicalOp,
        cache_ref: &RwLock<HashMap<(u64, String), Arc<HashMap<String, RoaringBitmap>>>>
    ) -> Result<Vec<(u64, RoaringBitmap)>> {
        let segment_results: Vec<Result<(u64, RoaringBitmap)>> = segment_tasks.par_iter()
            .map(|segment| {
                let bitmap = if let Some(writer) = &self.active_segment {
                    if writer.segment.id == segment.id {
                        writer.search(filters, filters_op)?
                    } else {
                        segment.search_thread_safe(filters, filters_op, cache_ref)?
                    }
                } else {
                    segment.search_thread_safe(filters, filters_op, cache_ref)?
                };
                Ok((segment.id, bitmap))
            })
            .collect();

        let mut matches = Vec::new();
        for res in segment_results {
            let (id, bitmap) = res?;
            if !bitmap.is_empty() {
                matches.push((id, bitmap));
            }
        }
        Ok(matches)
    }

    fn process_seg_sum_grouped(
        &self, 
        seg_id: u64, 
        bitmap: &RoaringBitmap, 
        field_name: &str, 
        required_fields: &[String], 
        expr: &crate::core::expression::Expr
    ) -> HashMap<String, f64> {
        let mut seg_group_sums = HashMap::new();
        let segment = if let Some(writer) = &self.active_segment {
            if writer.segment.id == seg_id { Some(&writer.segment) }
            else { self.immutable_segments.iter().find(|s| s.id == seg_id) }
        } else {
            self.immutable_segments.iter().find(|s| s.id == seg_id)
        };

        if let Some(s) = segment {
            let group_mmap = s.get_mmap_pair(field_name).ok();
            let mmaps: Vec<Option<Arc<(Mmap, Mmap)>>> = required_fields.iter()
                .map(|f| s.get_mmap_pair(f).ok())
                .collect();
            
            let mut context = HashMap::with_capacity(required_fields.len());
            
            for id in bitmap {
                let group_val = if let Some(m) = &group_mmap {
                    s.get_row_values_from_mmaps(id, &[Some(m.clone())]).pop().unwrap_or_default()
                } else { "Unknown".to_string() };

                let row_nums = s.get_row_numbers_from_mmaps(id, &mmaps);
                
                context.clear();
                for (i, val) in row_nums.into_iter().enumerate() {
                    context.insert(required_fields[i].clone(), val);
                }
                
                let val = crate::core::expression::evaluate(expr, &context);
                *seg_group_sums.entry(group_val).or_insert(0.0) += val;
            }
        }
        seg_group_sums
    }

    fn process_seg_sum_global(
        &self, 
        seg_id: u64, 
        bitmap: &RoaringBitmap, 
        required_fields: &[String], 
        expr: &crate::core::expression::Expr
    ) -> f64 {
        let segment = if let Some(writer) = &self.active_segment {
            if writer.segment.id == seg_id { Some(&writer.segment) }
            else { self.immutable_segments.iter().find(|s| s.id == seg_id) }
        } else {
            self.immutable_segments.iter().find(|s| s.id == seg_id)
        };

        let mut seg_sum = 0.0;
        if let Some(s) = segment {
            let mmaps: Vec<Option<Arc<(Mmap, Mmap)>>> = required_fields.iter()
                .map(|f| s.get_mmap_pair(f).ok())
                .collect();
            
            let mut context = HashMap::with_capacity(required_fields.len());
            for id in bitmap {
                let row_nums = s.get_row_numbers_from_mmaps(id, &mmaps);
                context.clear();
                for (i, val) in row_nums.into_iter().enumerate() {
                    context.insert(required_fields[i].clone(), val);
                }
                seg_sum += crate::core::expression::evaluate(expr, &context);
            }
        }
        seg_sum
    }
}
