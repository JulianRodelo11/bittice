use std::collections::HashMap;
use std::path::{Path, PathBuf};
use roaring::RoaringBitmap;
use anyhow::{Result, Context};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use crate::core::storage::manifest::SegmentMeta;

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

        Ok(Segment {
            id,
            path: path.to_path_buf(),
            is_immutable: true, // Assuming loaded segments are immutable by default unless opened for write
            min_max,
            deleted_bitmap,
            record_count: 0, // Should load from metadata
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
}
