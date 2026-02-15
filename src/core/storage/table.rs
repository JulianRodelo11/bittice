use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs;
use anyhow::{Result, Context};
use crate::core::storage::manifest::Manifest;
use crate::core::storage::segment::{Segment, SegmentWriter};
use crate::core::storage::wal::{Wal, WalOperation};

pub struct Table {
    pub name: String,
    pub base_path: PathBuf,
    manifest: Manifest,
    active_segment: Option<SegmentWriter>,
    // We might keep immutable segments loaded or load them on demand.
    // For now, let's keep track of them.
    immutable_segments: Vec<Segment>,
    wal: Wal,
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

        // Load Manifest
        let manifest_path = table_path.join("manifest.json");
        let manifest = if manifest_path.exists() {
            let file = fs::File::open(&manifest_path)?;
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader)?
        } else {
            Manifest::new()
        };

        // Open WAL
        let wal_path = table_path.join("wal.log");
        let wal = Wal::open(&wal_path)?;

        // Replay WAL if needed (TODO: Implement full replay logic later)
        // For now, we assume clean shutdown or empty WAL.
        
        let mut table = Table {
            name: name.to_string(),
            base_path: table_path,
            manifest,
            active_segment: None,
            immutable_segments: Vec::new(),
            wal,
        };

        table.load_segments()?;
        table.ensure_active_segment()?;

        Ok(table)
    }

    fn load_segments(&mut self) -> Result<()> {
        let segments_dir = self.base_path.join("segments");
        self.immutable_segments.clear();

        for seg_meta in &self.manifest.segments {
            // Skip active segment in this list, handled separately? 
            // Or treat all as segments and just mark one as active.
            // Let's load immutable ones here.
            if seg_meta.id != self.manifest.active_segment_id {
                 let seg_path = segments_dir.join(format!("seg_{:04}", seg_meta.id));
                 if seg_path.exists() {
                     let segment = Segment::load(&seg_path)?;
                     self.immutable_segments.push(segment);
                 }
            }
        }
        Ok(())
    }

    fn ensure_active_segment(&mut self) -> Result<()> {
        if self.active_segment.is_some() {
            return Ok(());
        }

        let segments_dir = self.base_path.join("segments");
        let active_id = self.manifest.active_segment_id;
        
        // Try to load existing active segment
        let seg_path = segments_dir.join(format!("seg_{:04}", active_id));
        let segment = if seg_path.exists() {
             let mut s = Segment::load(&seg_path)?;
             s.is_immutable = false;
             s
        } else {
            // Create new
            let s = Segment::new(active_id, &segments_dir);
            s.create_dirs()?;
            s
        };

        // Create writer wrapper
        self.active_segment = Some(SegmentWriter::new(segment));
        Ok(())
    }

    pub fn insert(&mut self, row_data: HashMap<String, String>) -> Result<()> {
        // 1. Append to WAL
        // For simplicity, we serialize the row as JSON bytes for the WAL
        let row_bytes = serde_json::to_vec(&row_data)?;
        // We need an ID for WAL op, assuming the row has an "_id" or we generate one.
        // For now, let's just generate a UUID if not present or use a placeholder.
        let id = row_data.get("_id").cloned().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        
        let op = WalOperation::Insert { id: id.clone(), data: row_bytes.clone() };
        self.wal.append(&op)?;

        // 2. Insert into Active Segment (In-Memory Buffer -> Disk)
        if let Some(writer) = &mut self.active_segment {
             writer.append_record(&row_data)?; 
        }

        Ok(())
    }

    pub fn flush_active_segment(&mut self) -> Result<()> {
        if let Some(mut writer) = self.active_segment.take() {
            // 1. Flush Writer to Disk
            writer.flush()?;
            
            // 2. Update Manifest State
            let meta = writer.segment.to_meta();
            self.manifest.add_segment(meta);
            
            // 3. Advance Active Segment ID
            self.manifest.active_segment_id += 1;
            // TODO: Ideally we track WAL sequence number here
            self.manifest.last_sequence_number += 1; 
            
            // 4. Atomic Manifest Commit
            let manifest_path = self.base_path.join("manifest.json");
            let temp_path = self.base_path.join("manifest.tmp");
            {
                let file = fs::File::create(&temp_path)?;
                serde_json::to_writer_pretty(&file, &self.manifest)?;
                file.sync_all()?;
            }
            fs::rename(&temp_path, &manifest_path)?;
            
            // 5. Truncate WAL (Safe now that data is in immutable segment and manifest is updated)
            self.wal.truncate()?;
            
            // 6. Move to Immutable List
            // We recreate the segment struct from the writer's segment to store it
            // (The writer consumed the segment, so we just use it)
            let mut immutable_seg = writer.segment;
            immutable_seg.is_immutable = true;
            self.immutable_segments.push(immutable_seg);
        }
        
        // Prepare new active segment
        self.ensure_active_segment()?;
        Ok(())
    }
}
