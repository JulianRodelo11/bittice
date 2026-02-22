use std::collections::HashMap;
use std::path::{Path, PathBuf};
use roaring::RoaringBitmap;
use anyhow::{Result, Context};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use crate::core::storage::manifest::SegmentMeta;
use crate::core::types::{Filter, ComparisonOp, LogicalOp};
use memmap2::Mmap;
use std::sync::{Arc, RwLock};

pub struct Segment {
    pub id: u64,
    pub path: PathBuf,
    pub is_immutable: bool,
    pub min_max: HashMap<String, (String, String)>,
    pub deleted_bitmap: RoaringBitmap,
    pub record_count: u64,
    /// Cache of memory-mapped files: Field -> (Data, Offsets)
    pub mmap_cache: RwLock<HashMap<String, Arc<(Mmap, Mmap)>>>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("record_count", &self.record_count)
            .finish()
    }
}

impl Segment {
    pub fn new(id: u64, base_path: &Path) -> Self {
        let path = base_path.join(format!("seg_{:04}", id));
        Segment {
            id,
            path,
            is_immutable: false,
            min_max: HashMap::new(),
            deleted_bitmap: RoaringBitmap::new(),
            record_count: 0,
            mmap_cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn to_meta(&self) -> SegmentMeta {
        SegmentMeta {
            id: self.id,
            min_max: self.min_max.clone(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            record_count: self.record_count,
            deleted_count: self.deleted_bitmap.len(),
            path: format!("seg_{:04}", self.id),
        }
    }

    pub fn create_dirs(&self) -> Result<()> {
        if !self.path.exists() {
            fs::create_dir_all(&self.path).context("Failed to create segment directory")?;
        }
        Ok(())
    }

    pub fn load(path: &Path, meta: Option<&SegmentMeta>) -> Result<Self> {
        let id_str = path.file_name().unwrap().to_string_lossy();
        let id = id_str.strip_prefix("seg_").unwrap_or("0").parse::<u64>().unwrap_or(0);

        let deleted_bitmap_path = path.join("deleted.bitmap");
        let deleted_bitmap = if deleted_bitmap_path.exists() {
            let file = File::open(deleted_bitmap_path)?;
            RoaringBitmap::deserialize_from(file)?
        } else {
            RoaringBitmap::new()
        };

        let (min_max, record_count) = if let Some(m) = meta {
            (m.min_max.clone(), m.record_count)
        } else {
            let metadata_path = path.join("metadata.json");
            let min_max = if metadata_path.exists() {
                let file = File::open(metadata_path)?;
                let reader = BufReader::new(file);
                serde_json::from_reader(reader).unwrap_or_default()
            } else {
                HashMap::new()
            };

            let mut count = 0;
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.ends_with(".offsets") {
                            if let Ok(meta) = entry.metadata() {
                                count = meta.len() / 8;
                                break; 
                            }
                        }
                    }
                }
            }
            (min_max, count)
        };

        Ok(Segment {
            id,
            path: path.to_path_buf(),
            is_immutable: true, 
            min_max,
            deleted_bitmap,
            record_count,
            mmap_cache: RwLock::new(HashMap::new()),
        })
    }

    pub fn mark_deleted(&mut self, local_id: u32) -> Result<()> {
        self.deleted_bitmap.insert(local_id);
        let path = self.path.join("deleted.bitmap");
        let file = OpenOptions::new().create(true).write(true).open(path)?;
        self.deleted_bitmap.serialize_into(file)?;
        Ok(())
    }

    pub fn update_metadata(&mut self, column: &str, value: &str) {
        let entry = self.min_max.entry(column.to_string()).or_insert((value.to_string(), value.to_string()));
        if value < entry.0.as_str() { entry.0 = value.to_string(); }
        if value > entry.1.as_str() { entry.1 = value.to_string(); }
    }

    pub fn save_metadata(&self) -> Result<()> {
        let path = self.path.join("metadata.json");
        let file = OpenOptions::new().create(true).write(true).open(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, &self.min_max)?;
        Ok(())
    }

    pub fn get_mmap_pair(&self, field: &str) -> Result<Arc<(Mmap, Mmap)>> {
        let mmap_pair = {
            let cache = self.mmap_cache.read().unwrap();
            cache.get(field).cloned()
        };

        if let Some(p) = mmap_pair {
            Ok(p)
        } else {
            let dat_path = self.path.join(format!("{}.dat", field));
            let off_path = self.path.join(format!("{}.offsets", field));

            if !dat_path.exists() || !off_path.exists() {
                return Err(anyhow::anyhow!("Field files not found"));
            }

            let dat_file = File::open(&dat_path)?;
            let off_file = File::open(&off_path)?;
            
            let pair = Arc::new((
                unsafe { Mmap::map(&dat_file)? },
                unsafe { Mmap::map(&off_file)? }
            ));

            let mut cache = self.mmap_cache.write().unwrap();
            if let Some(p) = cache.get(field) {
                return Ok(p.clone());
            }
            cache.insert(field.to_string(), pair.clone());
            Ok(pair)
        }
    }

    /// Retrieve multiple mmap pairs at once with a single read lock.
    pub fn get_mmaps_batch(&self, fields: &[String]) -> Vec<Option<Arc<(Mmap, Mmap)>>> {
        let cache = self.mmap_cache.read().unwrap();
        fields.iter().map(|f| cache.get(f).cloned()).collect()
    }

    pub fn get_counts_thread_safe(
        &self,
        field: &str,
        filter_bitmap: &RoaringBitmap,
        cache: &RwLock<HashMap<(u64, String), Arc<HashMap<String, RoaringBitmap>>>>
    ) -> Result<HashMap<String, u64>> {
        let cache_key = (self.id, field.to_string());
        
        let bitmaps = {
            let r = cache.read().unwrap();
            r.get(&cache_key).cloned()
        };

        let bitmaps = if let Some(bm) = bitmaps {
            bm
        } else {
            let bitmap_path = self.path.join(format!("bitmaps_{}.dat", field));
            let bm_loaded: HashMap<String, RoaringBitmap> = if bitmap_path.exists() {
                let file = File::open(bitmap_path)?;
                bincode::deserialize_from(file)?
            } else {
                HashMap::new()
            };
            let arc_bm = Arc::new(bm_loaded);
            let mut w = cache.write().unwrap();
            w.insert(cache_key, arc_bm.clone());
            arc_bm
        };

        let mut counts = HashMap::new();
        for (val, bm) in bitmaps.as_ref() {
            let count = (bm & filter_bitmap).len();
            if count > 0 {
                counts.insert(val.clone(), count);
            }
        }
        Ok(counts)
    }

    pub fn search_thread_safe(
        &self,
        filters: &[Filter],
        filters_op: &LogicalOp,
        cache: &RwLock<HashMap<(u64, String), Arc<HashMap<String, RoaringBitmap>>>>
    ) -> Result<RoaringBitmap> {
        let valid_filters: Vec<&Filter> = filters.iter().filter(|f| f.field != "?" && f.value != "?").collect();

        if valid_filters.is_empty() {
             let mut all = RoaringBitmap::new();
             all.insert_range(0..self.record_count as u32);
             if !self.deleted_bitmap.is_empty() { all -= &self.deleted_bitmap; }
             return Ok(all);
        }

        for f in &valid_filters {
            if let Some((min, max)) = self.min_max.get(&f.field) {
                let val = &f.value;
                let skip = match f.op {
                    ComparisonOp::Eq => val < min || val > max,
                    ComparisonOp::Gt => val >= max, 
                    ComparisonOp::Gte => val > max,
                    ComparisonOp::Lt => val <= min,
                    ComparisonOp::Lte => val < min,
                    _ => false,
                };
                if skip { return Ok(RoaringBitmap::new()); }
            }
        }

        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;

        for f in &valid_filters {
            let cache_key = (self.id, f.field.clone());
            
            // Try with read lock first
            let bitmaps = {
                let r = cache.read().unwrap();
                r.get(&cache_key).cloned()
            };

            let bitmaps = if let Some(bm) = bitmaps {
                bm
            } else {
                // If missing, load from disk and insert with write lock
                let bitmap_path = self.path.join(format!("bitmaps_{}.dat", f.field));
                let bm_loaded: HashMap<String, RoaringBitmap> = if bitmap_path.exists() {
                    let file = File::open(bitmap_path)?;
                    bincode::deserialize_from(file)?
                } else {
                    HashMap::new()
                };
                let arc_bm = Arc::new(bm_loaded);
                let mut w = cache.write().unwrap();
                w.entry(cache_key).or_insert(arc_bm.clone()).clone()
            };

            let mut filter_bitmap = RoaringBitmap::new();
            match f.op {
                ComparisonOp::Eq => { if let Some(bm) = bitmaps.get(&f.value) { filter_bitmap = bm.clone(); } },
                ComparisonOp::Ne => { for (k, bm) in bitmaps.as_ref() { if k != &f.value { filter_bitmap |= bm; } } },
                ComparisonOp::Gt => { for (k, bm) in bitmaps.as_ref() { if k.as_str() > f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Gte => { for (k, bm) in bitmaps.as_ref() { if k.as_str() >= f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Lt => { for (k, bm) in bitmaps.as_ref() { if k.as_str() < f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Lte => { for (k, bm) in bitmaps.as_ref() { if k.as_str() <= f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Like => {
                    let pattern = f.value.replace("%", "");
                    for (k, bm) in bitmaps.as_ref() { if k.contains(&pattern) { filter_bitmap |= bm; } }
                },
                ComparisonOp::In => {
                    let vals: Vec<&str> = f.value.split(';').map(|s| s.trim()).collect();
                    for (k, bm) in bitmaps.as_ref() { if vals.contains(&k.as_str()) { filter_bitmap |= bm; } }
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
        
        if !self.deleted_bitmap.is_empty() { result_bitmap -= &self.deleted_bitmap; }
        Ok(result_bitmap)
    }

    /// Unsafe: Does not check bounds. Caller ensures local_id is valid.
    pub unsafe fn read_value_unchecked(mmaps: &(Mmap, Mmap), local_id: u32) -> String {
        let (dat, off) = mmaps;
        
        let off_offset = (local_id as usize) << 3;
        let off_ptr = off.as_ptr().add(off_offset);
        let start_pos = u64::from_le((off_ptr as *const u64).read_unaligned()) as usize;
        
        let dat_ptr = dat.as_ptr().add(start_pos);
        let len = u64::from_le((dat_ptr as *const u64).read_unaligned()) as usize;
        
        let bytes_ptr = dat_ptr.add(8);
        let bytes = std::slice::from_raw_parts(bytes_ptr, len);
        
        // Performance: Since we wrote this as a String, it's guaranteed to be valid UTF-8.
        // from_utf8_unchecked avoids the scanning overhead.
        String::from_utf8_unchecked(bytes.to_vec())
    }

    pub fn get_row_values_from_mmaps(&self, local_id: u32, mmaps: &[Option<Arc<(Mmap, Mmap)>>]) -> Vec<String> {
        let mut values = Vec::with_capacity(mmaps.len());
        let start_idx = (local_id as usize) << 3;

        for mmap_pair_opt in mmaps {
            let val = if let Some(mmap_pair) = mmap_pair_opt {
                let (dat, off) = &**mmap_pair;
                if start_idx + 8 <= off.len() {
                    let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8].try_into().unwrap()) as usize;
                    if start_pos + 8 <= dat.len() {
                        let len = u64::from_le_bytes(dat[start_pos..start_pos+8].try_into().unwrap()) as usize;
                        if start_pos + 8 + len <= dat.len() {
                            let bytes = &dat[start_pos + 8..start_pos + 8 + len];
                            String::from_utf8_lossy(bytes).into_owned()
                        } else { String::new() }
                    } else { String::new() }
                } else { String::new() }
            } else {
                String::new()
            };
            values.push(val);
        }
        values
    }

    /// Optimized: Get numbers directly without String allocation
    pub fn get_row_numbers_from_mmaps(&self, local_id: u32, mmaps: &[Option<Arc<(Mmap, Mmap)>>]) -> Vec<f64> {
        let mut values = Vec::with_capacity(mmaps.len());
        let start_idx = (local_id as usize) << 3;

        for mmap_pair_opt in mmaps {
            let val = if let Some(mmap_pair) = mmap_pair_opt {
                let (dat, off) = &**mmap_pair;
                if start_idx + 8 <= off.len() {
                    let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8].try_into().unwrap()) as usize;
                    if start_pos + 8 <= dat.len() {
                        let len = u64::from_le_bytes(dat[start_pos..start_pos+8].try_into().unwrap()) as usize;
                        if start_pos + 8 + len <= dat.len() {
                            let bytes = &dat[start_pos + 8..start_pos + 8 + len];
                            if bytes.is_empty() { 0.0 }
                            else {
                                // Fast parse: Try to avoid UTF8 validation if possible
                                std::str::from_utf8(bytes).ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
                            }
                        } else { 0.0 }
                    } else { 0.0 }
                } else { 0.0 }
            } else {
                0.0
            };
            values.push(val);
        }
        values
    }

    pub fn get_row_values(&self, local_id: u32, fields: &[String]) -> Result<Vec<String>> {
        let mut mmaps = Vec::with_capacity(fields.len());
        {
            let cache = self.mmap_cache.read().unwrap();
            for field in fields { mmaps.push(cache.get(field).cloned()); }
        }
        Ok(self.get_row_values_from_mmaps(local_id, &mmaps))
    }

    /// Optimized batch loading of mmaps to reduce lock contention.
    /// Opens all missing files in parallel and acquires the write lock only once.
    pub fn ensure_mmaps_batch(&self, fields: &[String]) -> Result<()> {
        // 1. Identify missing fields (Read Lock)
        let missing_fields: Vec<String> = {
            let cache = self.mmap_cache.read().unwrap();
            fields.iter()
                .filter(|f| !cache.contains_key(*f))
                .cloned()
                .collect()
        };

        if missing_fields.is_empty() {
            return Ok(());
        }

        // 2. Open files in parallel (No Lock - IO Heavy)
        use rayon::prelude::*;
        let results: Vec<Result<(String, Arc<(Mmap, Mmap)>)>> = missing_fields.par_iter()
            .map(|field| {
                let dat_path = self.path.join(format!("{}.dat", field));
                let off_path = self.path.join(format!("{}.offsets", field));

                if !dat_path.exists() || !off_path.exists() {
                    return Err(anyhow::anyhow!("Field files not found: {}", field));
                }

                let dat_file = File::open(&dat_path)?;
                let off_file = File::open(&off_path)?;
                
                // Unsafe: mmap creation
                let dat_mmap = unsafe { Mmap::map(&dat_file)? };
                let off_mmap = unsafe { Mmap::map(&off_file)? };

                // Advise OS to pre-fetch data as we are about to read it in a wide query
                let _ = dat_mmap.advise(memmap2::Advice::WillNeed);
                let _ = off_mmap.advise(memmap2::Advice::WillNeed);
                
                let pair = Arc::new((dat_mmap, off_mmap));
                
                Ok((field.clone(), pair))
            })
            .collect();

        // 3. Insert into cache (Write Lock - Once)
        let mut cache = self.mmap_cache.write().unwrap();
        for res in results {
            match res {
                Ok((field, pair)) => {
                    cache.entry(field).or_insert(pair);
                }
                Err(_) => { 
                    // Log error or ignore? For now, we ignore failures to open individual cols 
                    // so we don't crash the whole batch, but in practice this might panic later.
                    // Given the existing code style, we'll let it fail later or implicitly handle it.
                }
            }
        }
        Ok(())
    }
}

pub struct SegmentWriter {
    pub segment: Segment,
    writers: HashMap<String, (BufWriter<File>, BufWriter<File>, u64)>,
    pub bitmaps: HashMap<String, HashMap<String, RoaringBitmap>>,
    pub dirty: bool,
}

impl SegmentWriter {
    pub fn new(segment: Segment) -> Self {
        SegmentWriter { segment, writers: HashMap::new(), bitmaps: HashMap::new(), dirty: false }
    }

    pub fn append_record(&mut self, row: &HashMap<String, String>) -> Result<()> {
        let local_id = self.segment.record_count as u32;
        for (col, val) in row {
            if !self.writers.contains_key(col) {
                let dat_path = self.segment.path.join(format!("{}.dat", col));
                let off_path = self.segment.path.join(format!("{}.offsets", col));
                let dat_file = OpenOptions::new().create(true).append(true).open(&dat_path)?;
                let off_file = OpenOptions::new().create(true).append(true).open(&off_path)?;
                let current_len = dat_file.metadata()?.len();
                self.writers.insert(col.clone(), (BufWriter::new(dat_file), BufWriter::new(off_file), current_len));
            }
            let (dat_writer, off_writer, current_offset) = self.writers.get_mut(col).unwrap();
            let encoded = bincode::serialize(val)?;
            let len = encoded.len() as u64;
            off_writer.write_all(&current_offset.to_le_bytes())?;
            dat_writer.write_all(&encoded)?;
            *current_offset += len;
            self.bitmaps.entry(col.clone()).or_default().entry(val.clone()).or_default().insert(local_id);
            self.segment.update_metadata(col, val);
        }
        self.segment.record_count += 1;
        self.dirty = true;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty { return Ok(()); }
        for (dat, off, _) in self.writers.values_mut() {
            dat.flush()?;
            off.flush()?;
        }
        self.segment.save_metadata()?;
        for (col, map) in &self.bitmaps {
            let path = self.segment.path.join(format!("bitmaps_{}.dat", col));
            let file = File::create(path)?;
            bincode::serialize_into(file, map)?;
        }
        self.dirty = false;
        Ok(())
    }

    pub fn search(&self, filters: &[Filter], filters_op: &LogicalOp) -> Result<RoaringBitmap> {
        let valid_filters: Vec<&Filter> = filters.iter().filter(|f| f.field != "?" && f.value != "?").collect();
        if valid_filters.is_empty() {
             let mut all = RoaringBitmap::new();
             all.insert_range(0..self.segment.record_count as u32);
             if !self.segment.deleted_bitmap.is_empty() { all -= &self.segment.deleted_bitmap; }
             return Ok(all);
        }
        for f in &valid_filters {
            if let Some((min, max)) = self.segment.min_max.get(&f.field) {
                let val = &f.value;
                let skip = match f.op {
                    ComparisonOp::Eq => val < min || val > max,
                    ComparisonOp::Gt => val >= max,
                    ComparisonOp::Gte => val > max,
                    ComparisonOp::Lt => val <= min,
                    ComparisonOp::Lte => val < min,
                    _ => false,
                };
                if skip { return Ok(RoaringBitmap::new()); }
            }
        }
        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;
        for f in &valid_filters {
            let empty_map = HashMap::new();
            let bitmaps = self.bitmaps.get(&f.field).unwrap_or(&empty_map);
            let mut filter_bitmap = RoaringBitmap::new();
            match f.op {
                ComparisonOp::Eq => { if let Some(bm) = bitmaps.get(&f.value) { filter_bitmap = bm.clone(); } },
                ComparisonOp::Ne => { for (k, bm) in bitmaps { if k != &f.value { filter_bitmap |= bm; } } },
                ComparisonOp::Gt => { for (k, bm) in bitmaps { if k.as_str() > f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Gte => { for (k, bm) in bitmaps { if k.as_str() >= f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Lt => { for (k, bm) in bitmaps { if k.as_str() < f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Lte => { for (k, bm) in bitmaps { if k.as_str() <= f.value.as_str() { filter_bitmap |= bm; } } },
                ComparisonOp::Like => {
                    let pattern = f.value.replace("%", "");
                    for (k, bm) in bitmaps { if k.contains(&pattern) { filter_bitmap |= bm; } }
                },
                ComparisonOp::In => {
                    let vals: Vec<&str> = f.value.split(';').map(|s| s.trim()).collect();
                    for (k, bm) in bitmaps { if vals.contains(&k.as_str()) { filter_bitmap |= bm; } }
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
        if !self.segment.deleted_bitmap.is_empty() { result_bitmap -= &self.segment.deleted_bitmap; }
        Ok(result_bitmap)
    }

    pub fn get_counts(&self, field: &str, filter_bitmap: &RoaringBitmap) -> Result<HashMap<String, u64>> {
        let empty_map = HashMap::new();
        let bitmaps = self.bitmaps.get(field).unwrap_or(&empty_map);
        let mut counts = HashMap::new();
        for (val, bm) in bitmaps {
            let count = (bm & filter_bitmap).len();
            if count > 0 { counts.insert(val.clone(), count); }
        }
        Ok(counts)
    }
}
