//! Segmented on-disk primary index with LRU-loaded segments.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ── Manifest ────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct Manifest {
    num_segments: u32,
    version: u8,
}

// ── Internal state (behind Mutex for &self reads from Table::search) ────────

struct IndexState {
    loaded: HashMap<u32, HashMap<String, (u64, u32)>>,
    access_order: VecDeque<u32>,
    dirty: HashSet<u32>,
}

// ── Struct principal ─────────────────────────────────────────────────────────

pub struct SegmentedPrimaryIndex {
    /// `.../<table>/primary/`
    base_path: PathBuf,
    num_segments: u32,
    state: Mutex<IndexState>,
    max_loaded_segments: usize,
}

impl SegmentedPrimaryIndex {
    pub fn num_segments(&self) -> u32 {
        self.num_segments
    }

    /// Sum of `len()` for segments currently in RAM (not total on disk).
    pub fn approx_len(&self) -> usize {
        let g = self.state.lock().expect("primary index poisoned");
        g.loaded.values().map(|m| m.len()).sum()
    }

    pub fn segment_id(pk: &str, num_segments: u32) -> u32 {
        if num_segments == 0 {
            return 0;
        }
        let h = fnv1a_64(pk.as_bytes());
        (h % num_segments as u64) as u32
    }

    pub(crate) fn primary_dir_from_table(table_path: &Path) -> PathBuf {
        table_path.join("primary")
    }

    /// Parse `BITTICE_PRIMARY_INDEX_SEGMENTS`: next power of two, min 1, max 4096.
    pub fn configured_num_segments() -> u32 {
        let raw = std::env::var("BITTICE_PRIMARY_INDEX_SEGMENTS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(256);
        // Clamp before next_power_of_two to avoid panic on oversized inputs.
        let v = raw.min(4096).max(1);
        let p = v.next_power_of_two();
        p.min(4096).max(1)
    }

    fn read_manifest(primary_dir: &Path) -> Result<Option<Manifest>> {
        let p = primary_dir.join("manifest.json");
        if !p.exists() {
            return Ok(None);
        }
        let file = fs::File::open(&p).with_context(|| format!("open {}", p.display()))?;
        let m: Manifest = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("parse manifest {}", p.display()))?;
        Ok(Some(m))
    }

    /// `table_path` is the table directory (`.../mirror/<entity>/<table>`).
    pub fn open(table_path: &Path, max_segments_in_ram: usize) -> Result<Self> {
        let base_path = Self::primary_dir_from_table(table_path);
        let num_segments = match Self::read_manifest(&base_path)? {
            Some(m) => m.num_segments.max(1),
            None => Self::configured_num_segments(),
        };
        let max_loaded_segments = max_segments_in_ram
            .min(num_segments as usize)
            .max(1);

        Ok(Self {
            base_path,
            num_segments,
            state: Mutex::new(IndexState {
                loaded: HashMap::new(),
                access_order: VecDeque::new(),
                dirty: HashSet::new(),
            }),
            max_loaded_segments,
        })
    }

    /// True if any on-disk segment under `primary/` deserializes to a non-empty map.
    pub fn disk_contains_any_entries(table_path: &Path) -> Result<bool> {
        let primary_dir = Self::primary_dir_from_table(table_path);
        let Some(manifest) = Self::read_manifest(&primary_dir)? else {
            return Ok(false);
        };
        let num = manifest.num_segments.max(1);
        for seg_id in 0..num {
            let m = Self::load_segment_map_from_disk(&primary_dir, seg_id)?;
            if !m.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn get(&self, pk: &str) -> Result<Option<(u64, u32)>> {
        let seg_id = Self::segment_id(pk, self.num_segments);
        let mut g = self.state.lock().expect("primary index poisoned");
        self.ensure_loaded(&mut g, seg_id)?;
        Ok(g.loaded.get(&seg_id).and_then(|m| m.get(pk).copied()))
    }

    pub fn insert(&self, pk: String, val: (u64, u32)) -> Result<()> {
        let seg_id = Self::segment_id(&pk, self.num_segments);
        let mut g = self.state.lock().expect("primary index poisoned");
        self.ensure_loaded(&mut g, seg_id)?;
        if let Some(m) = g.loaded.get_mut(&seg_id) {
            m.insert(pk, val);
            g.dirty.insert(seg_id);
        }
        Ok(())
    }

    pub fn remove(&self, pk: &str) -> Result<Option<(u64, u32)>> {
        let seg_id = Self::segment_id(pk, self.num_segments);
        let mut g = self.state.lock().expect("primary index poisoned");
        self.ensure_loaded(&mut g, seg_id)?;
        let prev = g
            .loaded
            .get_mut(&seg_id)
            .and_then(|m| m.remove(pk));
        if prev.is_some() {
            g.dirty.insert(seg_id);
        }
        Ok(prev)
    }

    pub fn contains_key(&self, pk: &str) -> Result<bool> {
        Ok(self.get(pk)?.is_some())
    }

    pub fn flush_dirty(&self) -> Result<()> {
        let mut g = self.state.lock().expect("primary index poisoned");
        let dirty: Vec<u32> = g.dirty.iter().copied().collect();
        for seg_id in dirty {
            self.flush_segment_locked(&mut g, seg_id)?;
        }
        Ok(())
    }

    pub fn flush_all(&self) -> Result<()> {
        let mut g = self.state.lock().expect("primary index poisoned");
        let loaded: Vec<u32> = g.loaded.keys().copied().collect();
        for seg_id in loaded {
            self.flush_segment_locked(&mut g, seg_id)?;
        }
        Ok(())
    }

    fn ensure_loaded(&self, g: &mut IndexState, seg_id: u32) -> Result<()> {
        if g.loaded.contains_key(&seg_id) {
            if let Some(pos) = g.access_order.iter().position(|&x| x == seg_id) {
                g.access_order.remove(pos);
            }
            g.access_order.push_front(seg_id);
            return Ok(());
        }

        while g.loaded.len() >= self.max_loaded_segments {
            let Some(evict_id) = g.access_order.pop_back() else {
                break;
            };
            if g.dirty.contains(&evict_id) {
                self.flush_segment_locked(g, evict_id)?;
            }
            g.loaded.remove(&evict_id);
        }

        let map = Self::load_segment_map_from_disk(&self.base_path, seg_id)?;
        g.loaded.insert(seg_id, map);
        g.access_order.push_front(seg_id);
        Ok(())
    }

    fn load_segment_map_from_disk(primary_dir: &Path, seg_id: u32) -> Result<HashMap<String, (u64, u32)>> {
        let path = primary_dir.join(format!("segment_{:02x}.idx", seg_id));
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let file = fs::File::open(&path).with_context(|| format!("open segment {}", path.display()))?;
        let reader = BufReader::new(file);
        let map: HashMap<String, (u64, u32)> = bincode::deserialize_from(reader)
            .with_context(|| format!("deserialize segment {}", path.display()))?;
        Ok(map)
    }

    fn flush_segment_locked(&self, g: &mut IndexState, seg_id: u32) -> Result<()> {
        let Some(map) = g.loaded.get(&seg_id) else {
            g.dirty.remove(&seg_id);
            return Ok(());
        };
        fs::create_dir_all(&self.base_path)
            .with_context(|| format!("create_dir_all {}", self.base_path.display()))?;
        if !self.base_path.join("manifest.json").exists() {
            self.write_manifest_to_disk()?;
        }
        let final_path = self.base_path.join(format!("segment_{:02x}.idx", seg_id));
        let tmp_path = self.base_path.join(format!("segment_{:02x}.idx.tmp", seg_id));
        {
            let file = fs::File::create(&tmp_path)
                .with_context(|| format!("create tmp segment {}", tmp_path.display()))?;
            let writer = BufWriter::new(file);
            bincode::serialize_into(writer, map)
                .with_context(|| format!("serialize segment {}", final_path.display()))?;
        }
        fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("rename segment {}", final_path.display()))?;
        g.dirty.remove(&seg_id);
        Ok(())
    }

    fn write_manifest_to_disk(&self) -> Result<()> {
        fs::create_dir_all(&self.base_path)
            .with_context(|| format!("create_dir_all {}", self.base_path.display()))?;
        let path = self.base_path.join("manifest.json");
        let tmp = self.base_path.join("manifest.json.tmp");
        let m = Manifest {
            num_segments: self.num_segments,
            version: 1,
        };
        {
            let f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            serde_json::to_writer_pretty(&f, &m).with_context(|| format!("write {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn iter_all_segments<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&str, (u64, u32)) -> Result<()>,
    {
        for seg_id in 0..self.num_segments {
            let mut g = self.state.lock().expect("primary index poisoned");
            self.ensure_loaded(&mut g, seg_id)?;
            let snapshot: Vec<(String, (u64, u32))> = g
                .loaded
                .get(&seg_id)
                .map(|m| {
                    m.iter()
                        .map(|(k, &v)| (k.clone(), v))
                        .collect()
                })
                .unwrap_or_default();
            drop(g);
            for (k, v) in snapshot {
                f(k.as_str(), v)?;
            }
        }
        Ok(())
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

pub fn migrate_legacy_index(table_path: &Path, num_segments: u32) -> Result<bool> {
    let legacy_path = table_path.join("primary.idx");
    let primary_dir = SegmentedPrimaryIndex::primary_dir_from_table(table_path);
    let manifest_path = primary_dir.join("manifest.json");

    if !legacy_path.exists() {
        return Ok(false);
    }

    if manifest_path.exists() {
        if legacy_path.exists() {
            let backup = table_path.join("primary.idx.migrated");
            if !backup.exists() {
                let _ = fs::rename(&legacy_path, &backup);
            }
        }
        return Ok(false);
    }

    let file = match fs::File::open(&legacy_path) {
        Ok(f) => f,
        Err(e) => {
            warn!("migrate_legacy_index: cannot open {}: {}", legacy_path.display(), e);
            return Ok(false);
        }
    };
    let reader = BufReader::new(file);
    let full: HashMap<String, (u64, u32)> = match bincode::deserialize_from(reader) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "migrate_legacy_index: bincode decode failed for {}: {}",
                legacy_path.display(),
                e
            );
            return Ok(false);
        }
    };

    let n_keys = full.len();
    fs::create_dir_all(&primary_dir)
        .with_context(|| format!("create {}", primary_dir.display()))?;

    let mut by_seg: Vec<HashMap<String, (u64, u32)>> =
        (0..num_segments).map(|_| HashMap::new()).collect();
    for (pk, val) in full {
        let sid = SegmentedPrimaryIndex::segment_id(&pk, num_segments) as usize;
        by_seg[sid].insert(pk, val);
    }

    for seg_id in 0..num_segments {
        let final_path = primary_dir.join(format!("segment_{:02x}.idx", seg_id));
        let tmp_path = primary_dir.join(format!("segment_{:02x}.idx.tmp", seg_id));
        {
            let file = fs::File::create(&tmp_path)
                .with_context(|| format!("create {}", tmp_path.display()))?;
            let w = BufWriter::new(file);
            bincode::serialize_into(w, &by_seg[seg_id as usize])
                .with_context(|| format!("serialize {}", final_path.display()))?;
        }
        fs::rename(&tmp_path, &final_path).with_context(|| format!("rename {}", final_path.display()))?;
    }

    let manifest = Manifest {
        num_segments,
        version: 1,
    };
    let mpath_tmp = primary_dir.join("manifest.json.tmp");
    {
        let f = fs::File::create(&mpath_tmp).context("create manifest tmp")?;
        serde_json::to_writer_pretty(&f, &manifest)?;
    }
    fs::rename(&mpath_tmp, &manifest_path).context("rename manifest")?;

    let migrated_name = table_path.join("primary.idx.migrated");
    fs::rename(&legacy_path, &migrated_name).with_context(|| {
        format!(
            "rename legacy index to {}",
            migrated_name.display()
        )
    })?;

    let table = table_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    let entity = table_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    info!(
        "Migrated primary index for {}/{}: {} keys → {} segments",
        entity, table, n_keys, num_segments
    );

    Ok(true)
}
