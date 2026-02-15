use std::collections::HashMap;
use std::path::{Path, PathBuf};
use roaring::RoaringBitmap;
use anyhow::{Result, Context};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use crate::core::storage::manifest::SegmentMeta;
use crate::core::types::{Filter, ComparisonOp, LogicalOp};

#[derive(Debug)]
pub struct Segment {
    pub id: u64,
    pub path: PathBuf,
    pub is_immutable: bool,
    pub min_max: HashMap<String, (String, String)>,
    pub deleted_bitmap: RoaringBitmap,
    pub record_count: u64,
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
            path: format!("seg_{:04}", self.id),
        }
    }

    pub fn create_dirs(&self) -> Result<()> {
        if !self.path.exists() {
            fs::create_dir_all(&self.path).context("Failed to create segment directory")?;
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let id_str = path.file_name().unwrap().to_string_lossy();
        let id = id_str.strip_prefix("seg_").unwrap_or("0").parse::<u64>().unwrap_or(0);

        // Load deleted bitmap
        let deleted_bitmap_path = path.join("deleted.bitmap");
        let deleted_bitmap = if deleted_bitmap_path.exists() {
            let file = File::open(deleted_bitmap_path)?;
            RoaringBitmap::deserialize_from(file)?
        } else {
            RoaringBitmap::new()
        };

        // Load metadata (min/max) if available
        let metadata_path = path.join("metadata.json");
        let min_max = if metadata_path.exists() {
            let file = File::open(metadata_path)?;
            let reader = BufReader::new(file);
            serde_json::from_reader(reader).unwrap_or_default()
        } else {
            HashMap::new()
        };

        let mut record_count = 0;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".offsets") {
                        if let Ok(meta) = entry.metadata() {
                            record_count = meta.len() / 8;
                            break; 
                        }
                    }
                }
            }
        }

        Ok(Segment {
            id,
            path: path.to_path_buf(),
            is_immutable: true, 
            min_max,
            deleted_bitmap,
            record_count, 
        })
    }

    pub fn mark_deleted(&mut self, local_id: u32) -> Result<()> {
        self.deleted_bitmap.insert(local_id);
        // Persist immediately for durability (or batch it)
        let path = self.path.join("deleted.bitmap");
        let file = OpenOptions::new().create(true).write(true).open(path)?;
        self.deleted_bitmap.serialize_into(file)?;
        Ok(())
    }

    pub fn update_metadata(&mut self, column: &str, value: &str) {
        let entry = self.min_max.entry(column.to_string()).or_insert((value.to_string(), value.to_string()));
        if value < entry.0.as_str() {
            entry.0 = value.to_string();
        }
        if value > entry.1.as_str() {
            entry.1 = value.to_string();
        }
    }

    pub fn save_metadata(&self) -> Result<()> {
        let path = self.path.join("metadata.json");
        let file = OpenOptions::new().create(true).write(true).open(path)?;
        let writer = BufWriter::new(file);
        serde_json::to_writer(writer, &self.min_max)?;
        Ok(())
    }

    pub fn search(
        &self,
        filters: &[Filter],
        filters_op: &LogicalOp,
        cache: &mut HashMap<(u64, String), HashMap<String, RoaringBitmap>>
    ) -> Result<RoaringBitmap> {
        
        let valid_filters: Vec<&Filter> = filters.iter().filter(|f| f.field != "?" && f.value != "?").collect();

        if valid_filters.is_empty() {
             let mut all = RoaringBitmap::new();
             all.insert_range(0..self.record_count as u32);
             if !self.deleted_bitmap.is_empty() {
                 all -= &self.deleted_bitmap;
             }
             return Ok(all);
        }

        // 1. Pruning (Data Skipping)
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
                if skip {
                    return Ok(RoaringBitmap::new()); // Pruned!
                }
            }
        }

        // 2. Filter Execution
        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;

        for f in &valid_filters {
            // Load Bitmaps if not in cache
            let cache_key = (self.id, f.field.clone());
            if !cache.contains_key(&cache_key) {
                let bitmap_path = self.path.join(format!("bitmaps_{}.dat", f.field));
                if bitmap_path.exists() {
                    let file = File::open(bitmap_path)?;
                    let bitmaps: HashMap<String, RoaringBitmap> = bincode::deserialize_from(file)?;
                    cache.insert(cache_key.clone(), bitmaps);
                } else {
                    cache.insert(cache_key.clone(), HashMap::new());
                }
            }

            let bitmaps = cache.get(&cache_key).unwrap();
            let mut filter_bitmap = RoaringBitmap::new();

            match f.op {
                ComparisonOp::Eq => {
                    if let Some(bm) = bitmaps.get(&f.value) {
                        filter_bitmap = bm.clone();
                    }
                },
                ComparisonOp::Ne => {
                    for (k, bm) in bitmaps {
                        if k != &f.value {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Gt => {
                    for (k, bm) in bitmaps {
                        if k.as_str() > f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Gte => {
                    for (k, bm) in bitmaps {
                        if k.as_str() >= f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Lt => {
                    for (k, bm) in bitmaps {
                        if k.as_str() < f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Lte => {
                    for (k, bm) in bitmaps {
                        if k.as_str() <= f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Like => {
                    let pattern = f.value.replace("%", "");
                    for (k, bm) in bitmaps {
                        if k.contains(&pattern) {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::In => {
                    let vals: Vec<&str> = f.value.split(',').map(|s| s.trim()).collect();
                    for (k, bm) in bitmaps {
                        if vals.contains(&k.as_str()) {
                            filter_bitmap |= bm;
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
        
        // Remove deleted records
        if !self.deleted_bitmap.is_empty() {
            result_bitmap -= &self.deleted_bitmap;
        }
        
        Ok(result_bitmap)
    }

    pub fn get_row(&self, local_id: u32, fields: &[String]) -> Result<HashMap<String, String>> {
        let mut row = HashMap::new();
        for field in fields {
            let dat_path = self.path.join(format!("{}.dat", field));
            let off_path = self.path.join(format!("{}.offsets", field));

            if !dat_path.exists() || !off_path.exists() {
                continue; // Field not found, treat as null/empty
            }

            let mut off_file = File::open(&off_path)?;
            let mut dat_file = File::open(&dat_path)?;

            // Read offset
            use std::io::{Seek, Read};
            off_file.seek(std::io::SeekFrom::Start((local_id as u64) * 8))?;
            let mut start_buf = [0u8; 8];
            off_file.read_exact(&mut start_buf)?;
            let start_pos = u64::from_le_bytes(start_buf);

            // Read Data
            dat_file.seek(std::io::SeekFrom::Start(start_pos))?;
            
            // We assume bincode format: [len (8 bytes)][utf8 bytes]
            let val: String = bincode::deserialize_from(&dat_file)?;
            row.insert(field.clone(), val);
        }
        Ok(row)
    }
}

pub struct SegmentWriter {
    pub segment: Segment,
    // Column -> (DataWriter, OffsetWriter, CurrentOffset)
    writers: HashMap<String, (BufWriter<File>, BufWriter<File>, u64)>,
    // In-memory bitmaps for active segment: Column -> Value -> Bitmap
    pub bitmaps: HashMap<String, HashMap<String, RoaringBitmap>>,
}

impl SegmentWriter {
    pub fn new(segment: Segment) -> Self {
        SegmentWriter {
            segment,
            writers: HashMap::new(),
            bitmaps: HashMap::new(),
        }
    }

    pub fn append_record(&mut self, row: &HashMap<String, String>) -> Result<()> {
        let local_id = self.segment.record_count as u32;

        for (col, val) in row {
            // 1. Ensure writers exist
            if !self.writers.contains_key(col) {
                let dat_path = self.segment.path.join(format!("{}.dat", col));
                let off_path = self.segment.path.join(format!("{}.offsets", col));

                let dat_file = OpenOptions::new().create(true).append(true).open(&dat_path)?;
                let off_file = OpenOptions::new().create(true).append(true).open(&off_path)?;

                let current_len = dat_file.metadata()?.len();
                
                self.writers.insert(col.clone(), (
                    BufWriter::new(dat_file), 
                    BufWriter::new(off_file),
                    current_len
                ));
            }

            let (dat_writer, off_writer, current_offset) = self.writers.get_mut(col).unwrap();

            // 2. Write Data
            // We write raw bytes or length-prefixed? 
            // Current engine uses `bincode::serialize(val)`. Let's stick to that for compatibility.
            let encoded = bincode::serialize(val)?; // Serialization includes length if String? No, bincode string is len+utf8.
            let len = encoded.len() as u64;

            // 3. Write Offset
            // Format: start_pos (u64). End pos is inferred from next start pos or file len.
            // Wait, query.rs uses:
            // let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8]...
            // let end_pos = ... off[start_idx+8..] ...
            // So we just write the *start* position of this record.
            off_writer.write_all(&current_offset.to_le_bytes())?;

            dat_writer.write_all(&encoded)?;
            *current_offset += len;

            // 4. Update Bitmaps
            self.bitmaps.entry(col.clone()).or_default()
                .entry(val.clone()).or_default()
                .insert(local_id);
            
            // 5. Update Metadata
            self.segment.update_metadata(col, val);
        }
        
        self.segment.record_count += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        for (dat, off, _) in self.writers.values_mut() {
            dat.flush()?;
            off.flush()?;
        }
        self.segment.save_metadata()?;
        // Also dump bitmaps? For active segment, maybe we keep them in memory or dump to temp files?
        // For now, let's dump them to `bitmaps_{col}.dat` so query engine can find them.
        for (col, map) in &self.bitmaps {
            let path = self.segment.path.join(format!("bitmaps_{}.dat", col));
            let file = File::create(path)?;
            bincode::serialize_into(file, map)?;
        }
        Ok(())
    }

    pub fn search(&self, filters: &[Filter], filters_op: &LogicalOp) -> Result<RoaringBitmap> {
        // 1. Pruning
        for f in filters {
            if f.field == "?" || f.value == "?" { continue; }
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
                if skip {
                    return Ok(RoaringBitmap::new());
                }
            }
        }

        // 2. Filter Execution
        let mut result_bitmap = RoaringBitmap::new();
        let mut first = true;

        for f in filters {
            if f.field == "?" || f.value == "?" { continue; }

            // Use in-memory bitmaps
            let empty_map = HashMap::new();
            let bitmaps = self.bitmaps.get(&f.field).unwrap_or(&empty_map);
            let mut filter_bitmap = RoaringBitmap::new();

            match f.op {
                ComparisonOp::Eq => {
                    if let Some(bm) = bitmaps.get(&f.value) {
                        filter_bitmap = bm.clone();
                    }
                },
                ComparisonOp::Ne => {
                    for (k, bm) in bitmaps {
                        if k != &f.value {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Gt => {
                    for (k, bm) in bitmaps {
                        if k.as_str() > f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Gte => {
                    for (k, bm) in bitmaps {
                        if k.as_str() >= f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Lt => {
                    for (k, bm) in bitmaps {
                        if k.as_str() < f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Lte => {
                    for (k, bm) in bitmaps {
                        if k.as_str() <= f.value.as_str() {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::Like => {
                    let pattern = f.value.replace("%", "");
                    for (k, bm) in bitmaps {
                        if k.contains(&pattern) {
                            filter_bitmap |= bm;
                        }
                    }
                },
                ComparisonOp::In => {
                    let vals: Vec<&str> = f.value.split(',').map(|s| s.trim()).collect();
                    for (k, bm) in bitmaps {
                        if vals.contains(&k.as_str()) {
                            filter_bitmap |= bm;
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

        // Remove deleted records
        if !self.segment.deleted_bitmap.is_empty() {
            result_bitmap -= &self.segment.deleted_bitmap;
        }

        Ok(result_bitmap)
    }
}
