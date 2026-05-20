use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::sync::{Arc, RwLock as StdRwLock};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use memmap2::Mmap;
use anyhow::{Result, Context};
use roaring::RoaringBitmap;
use crate::core::storage::canonical::canonical_bytes;
use crate::core::storage::exact_index::ExactIndex;
use crate::core::storage::manifest::Manifest;
use crate::core::storage::primary_index::PrimaryIndex;
use crate::core::storage::primary_index_io;
use crate::core::storage::segment::{Segment, SegmentWriter};
use crate::core::storage::wal::{Wal, WalOperation};
use crate::core::types::{compare_filter_value, Filter, LogicalOp, OrderBy, QueryResult, SortDirection};
use rayon::prelude::*;
use std::cmp::Ordering;
use tracing::{info, debug, warn};
use unicode_normalization::UnicodeNormalization;

pub struct Table {
    pub name: String,
    pub base_path: PathBuf,
    pub manifest: Manifest,
    active_segment: Option<SegmentWriter>,
    immutable_segments: Vec<Segment>,
    wal: Wal,
    /// External ID -> (Segment ID, Local ID)
    pub primary_index: PrimaryIndex,
    /// Field name → mutable per-field index.  The inner `RwLock` lets `merge_exact_indexes_for_segment`
    /// add a freshly-rotated segment in place (O(values touched)) instead of cloning the entire
    /// `ExactIndex` (millions of allocations on high-cardinality fields).  Readers take the inner
    /// read lock briefly and scope it to a single value lookup.
    exact_index_cache: StdRwLock<HashMap<String, Arc<StdRwLock<ExactIndex>>>>,
    flush_count_since_index_save: u32,
    /// Set by TableManager when this Table lost a cache-dedup race and is about to be dropped.
    /// Prevents Drop::close from calling save_manifest and racing with the winning Table instance.
    discarded: bool,
}

// Structures for the Heap
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

impl Drop for Table {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl Table {
    /// Mark this Table as discarded — it lost a TableManager cache-dedup race and will be dropped
    /// without having been used.  Prevents Drop::close from calling save_manifest and racing with
    /// the winning Table instance's concurrent manifest writes.
    pub fn discard(&mut self) {
        self.discarded = true;
    }

    pub fn immutable_segment_count(&self) -> usize {
        self.immutable_segments.len()
    }

    pub fn close(&mut self) -> Result<()> {
        // If this instance was discarded (lost a concurrent open race in TableManager), skip all
        // disk writes — the winning instance owns the on-disk state.
        if self.discarded {
            return Ok(());
        }
        if let Some(mut writer) = self.active_segment.take() {
            let _ = writer.flush();
            // Same reason as in `flush_active_segment`: `SegmentWriter::flush`
            // only writes bitmaps every N flushes; close() is the last chance
            // before the active segment becomes a sealed immutable segment,
            // so force the final persist.
            let _ = writer.persist_bitmaps();
        }
        let _ = self.wal.flush_writes();
        // Persist every immutable segment's `deleted.bitmap`. Without this,
        // rows tombstoned by `delete()` since the last `flush_active_segment`
        // sit only in memory — close() (or process exit through close()) drops
        // them, and on restart the secondary_exact path finds the still-live
        // entry in the old segment as well as the new copy in the rotated
        // segment, producing ghost duplicates.
        let _ = self.persist_all_deleted_bitmaps();
        if !self.primary_index.is_empty() {
            let _ = self.save_manifest();
            let _ = self.save_primary_index();
            // Persist the in-memory exact_index — `merge_exact_indexes_for_segment` skips
            // disk I/O on the CDC hot path to avoid holding the write lock for seconds, so
            // close() is the safety net that ensures the on-disk index reflects the latest
            // rotated segments before the table leaves RAM.
            let _ = self.persist_all_exact_indexes();
            // Truncate WAL now that primary_index is persisted and the active segment
            // is flushed. Without this, large WAL files (100MB–300MB) accumulate for
            // high-write tables and are fully replayed on every restart, inflating both
            // startup time and peak RAM. The WAL is redundant once primary_index is saved.
            let _ = self.wal.truncate();
        }
        for seg in &mut self.immutable_segments {
            seg.mmap_cache.write().unwrap().clear();
            seg.mmap_access_order.write().unwrap().clear();
            seg.bitmap_cache.write().unwrap().clear();
        }
        self.exact_index_cache.write().unwrap().clear();
        Ok(())
    }

    pub fn open(base_path: &Path, name: &str) -> Result<Self> {
        let table_path = base_path.join(name);
        if !table_path.exists() { fs::create_dir_all(&table_path).context("Failed to create table directory")?; }
        let segments_dir = table_path.join("segments");
        if !segments_dir.exists() { fs::create_dir_all(&segments_dir).context("Failed to create segments directory")?; }
        let manifest_path = table_path.join("manifest.json");
        let manifest: Manifest = if manifest_path.exists() {
            let file = fs::File::open(&manifest_path)?;
            let reader = std::io::BufReader::new(file);
            serde_json::from_reader(reader)?
        } else { Manifest::new() };
        let wal_path = table_path.join("wal.log");
        let wal = Wal::open(&wal_path)?;
        let mut table = Table {
            name: name.to_string(),
            base_path: table_path,
            manifest,
            active_segment: None,
            immutable_segments: Vec::new(),
            wal,
            primary_index: PrimaryIndex::new(),
            exact_index_cache: StdRwLock::new(HashMap::new()),
            flush_count_since_index_save: 0,
            discarded: false,
        };
        table.load_segments()?;
        table.ensure_active_segment()?;
        table.load_primary_index()?;
        // Sync primary_index with whatever the active segment's writer.bitmaps
        // contain after rehydrate. If primary_index wasn't saved between the
        // last segment append and the crash, it lags behind the data on disk.
        // Without registering those PKs here, WAL replay's idempotent insert
        // can't recognize them as already-applied and re-appends — producing
        // duplicates that reconcile_orphan_rows then has to clean up
        // (sometimes incompletely, per the v0.1.133 history).
        table.sync_primary_index_to_active_bitmaps();
        table.replay_wal()?;
        // After WAL replay finishes, walk the segments and dedupe by physical
        // ordering (highest seg_id wins per PK) — kills any orphan left behind
        // by a SIGKILL or partial flush.
        if let Err(e) = table.reconcile_orphan_rows() {
            warn!(
                "Table '{}': reconcile_orphan_rows failed ({}); some segments may still hold stale rows.",
                table.name, e
            );
        }
        // If WAL replay appended any rows to the active segment, rotate it so
        // those rows enter `immutable_segments` and `secondary_exact` (the
        // saved-op query path uses the exact index for filters and doesn't
        // scan active by itself). Without this, a query right after a SIGKILL
        // recovery — before the next CDC event triggers a natural rotation —
        // returns zero rows even though the data is durably on disk.
        let active_has_rows = table
            .active_segment
            .as_ref()
            .map(|w| w.segment.record_count > 0)
            .unwrap_or(false);
        if active_has_rows {
            if let Err(e) = table.flush_active_segment() {
                warn!(
                    "Table '{}': post-replay flush_active_segment failed ({}); replayed rows may be \
                     invisible to secondary_exact until the next natural rotation.",
                    table.name, e
                );
            }
        }
        // One-time cleanup: remove `exact_<PK>.idx` left over from sessions before the PK
        // skip was added.  The file is hundreds of MB on tables with many distinct keys and
        // has no benefit (PK Eq/IN now go through `primary_index`).
        if !table.manifest.primary_key.is_empty() {
            let stale = table.exact_index_path(&table.manifest.primary_key);
            if stale.exists() {
                let _ = fs::remove_file(&stale);
            }
        }
        Ok(table)
    }

    /// For every PK present in the active segment's rehydrated bitmaps that
    /// isn't yet in primary_index, register it. This bridges the gap between
    /// the persisted primary_index (saved periodically) and the active
    /// segment's data files (durable per CDC flush). After this runs,
    /// `replay_wal`'s idempotent insert can correctly recognize rows that
    /// are already on disk and skip re-appending them.
    fn sync_primary_index_to_active_bitmaps(&mut self) {
        let pk_field = if self.manifest.primary_key.is_empty() {
            "PK".to_string()
        } else {
            self.manifest.primary_key.clone()
        };
        let (seg_id, additions): (u64, Vec<(String, u32)>) = match &self.active_segment {
            Some(writer) => {
                let pk_bitmaps = match writer.bitmaps.get(&pk_field) {
                    Some(b) => b,
                    None => return,
                };
                let mut adds: Vec<(String, u32)> = Vec::new();
                for (pk_val, bitmap) in pk_bitmaps {
                    // A PK may appear at multiple local_ids if the segment had
                    // an in-place update before the crash. Pick the highest —
                    // that is the latest write. Older locations will be
                    // tombstoned later by reconcile_orphan_rows.
                    if let Some(max_local) = bitmap.iter().max() {
                        if writer.segment.deleted_bitmap.contains(max_local) {
                            continue;
                        }
                        adds.push((pk_val.clone(), max_local));
                    }
                }
                (writer.segment.id, adds)
            }
            None => return,
        };
        for (pk, local_id) in additions {
            if self.primary_index.get(&pk).is_none() {
                self.primary_index.insert(&pk, (seg_id, local_id));
            }
        }
    }

    /// Reconciles "ghost" rows surviving across SIGKILL/crash boundaries.
    /// The data on disk — not `primary_index` — is the source of truth:
    /// `primary_index` is persisted periodically, so right after a hard kill
    /// it can lag behind the actual rows on disk. Using primary_index to
    /// decide who's-the-live-row could mark the legitimately current row as
    /// dead and leave a stale one alive. Instead, this pass uses *physical
    /// segment ordering*: for any PK that appears live in multiple segments,
    /// the row in the highest `(seg_id, local_id)` wins, the others get
    /// tombstoned. `primary_index` is then re-pointed at the winner.
    ///
    /// Segment IDs increase monotonically as rotations happen, and the
    /// active segment always has the highest ID. So the "newest physical
    /// location" really is the most-recent version regardless of how stale
    /// `primary_index` got.
    fn reconcile_orphan_rows(&mut self) -> Result<()> {
        let pk_field = if self.manifest.primary_key.is_empty() {
            "PK".to_string()
        } else {
            self.manifest.primary_key.clone()
        };

        // Phase 1: collect every live (pk → [(seg_id, local_id), ...]) location.
        let mut all_locations: HashMap<String, Vec<(u64, u32)>> = HashMap::new();

        // Immutable segments — read persisted PK bitmaps from disk.
        for seg in &self.immutable_segments {
            let bitmap_path = seg.path.join(format!("bitmaps_{}.dat", pk_field));
            if !bitmap_path.exists() {
                continue;
            }
            let file = match fs::File::open(&bitmap_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = BufReader::new(file);
            let bitmaps: HashMap<String, RoaringBitmap> = match bincode::deserialize_from(reader) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for (pk_val, bitmap) in &bitmaps {
                for local_id in bitmap.iter() {
                    if seg.deleted_bitmap.contains(local_id) {
                        continue;
                    }
                    all_locations
                        .entry(pk_val.clone())
                        .or_default()
                        .push((seg.id, local_id));
                }
            }
        }

        // Active segment — bitmaps live in memory, not on disk yet.
        if let Some(writer) = &self.active_segment {
            if let Some(pk_bitmaps) = writer.bitmaps.get(&pk_field) {
                for (pk_val, bitmap) in pk_bitmaps {
                    for local_id in bitmap.iter() {
                        if writer.segment.deleted_bitmap.contains(local_id) {
                            continue;
                        }
                        all_locations
                            .entry(pk_val.clone())
                            .or_default()
                            .push((writer.segment.id, local_id));
                    }
                }
            }
        }

        // Phase 2: per PK pick the winner = max (seg_id, local_id). Build
        // a per-segment list of orphans to tombstone.
        let mut orphans_by_seg: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut latest_per_pk: HashMap<String, (u64, u32)> = HashMap::new();
        for (pk, locations) in &all_locations {
            // unwrap safe: vec was created via `or_default().push(...)` so non-empty
            let &latest = locations.iter().max().expect("locations vec non-empty");
            latest_per_pk.insert(pk.clone(), latest);
            if locations.len() < 2 {
                continue;
            }
            for &(seg_id, local_id) in locations {
                if (seg_id, local_id) != latest {
                    orphans_by_seg.entry(seg_id).or_default().push(local_id);
                }
            }
        }

        // Phase 3: mark + persist tombstones.
        for (seg_id, orphans) in &orphans_by_seg {
            // Active segment is mutable separately from immutable_segments.
            if let Some(writer) = &mut self.active_segment {
                if writer.segment.id == *seg_id {
                    for local_id in orphans {
                        writer.segment.mark_deleted(*local_id)?;
                    }
                    writer.segment.persist_deleted_bitmap()?;
                    info!(
                        "Table '{}': reconcile_orphan_rows tombstoned {} stale row(s) in active seg {}",
                        self.name,
                        orphans.len(),
                        seg_id
                    );
                    continue;
                }
            }
            if let Some(seg) = self.immutable_segments.iter_mut().find(|s| s.id == *seg_id) {
                for local_id in orphans {
                    seg.mark_deleted(*local_id)?;
                }
                seg.persist_deleted_bitmap()?;
                info!(
                    "Table '{}': reconcile_orphan_rows tombstoned {} stale row(s) in seg {}",
                    self.name,
                    orphans.len(),
                    seg_id
                );
            }
        }

        // Phase 4: bring `primary_index` in line with the physical truth we
        // just established. Anything pointing at a now-tombstoned row gets
        // re-pointed at the winner; PKs that have a winner but weren't in
        // primary_index get added. PKs that disappeared from segments
        // entirely (none of `latest_per_pk`) are not touched here — that
        // would be a deletion-without-tombstone case which compact() owns.
        for (pk, latest) in &latest_per_pk {
            let needs_update = self
                .primary_index
                .get(pk)
                .map(|cur| cur != *latest)
                .unwrap_or(true);
            if needs_update {
                self.primary_index.insert(pk, *latest);
            }
        }

        Ok(())
    }

    fn replay_wal(&mut self) -> Result<()> {
        // ALWAYS replay. Earlier this method had an early-return when
        // `primary_index` on disk was non-empty: that was a wrong
        // optimization. `primary_index` is saved periodically while WAL
        // entries are appended on every event; between a primary_index save
        // and the next checkpoint, the WAL holds Delete/Insert pairs whose
        // tombstones live only in-memory in the segments. A SIGKILL in that
        // window combined with the old early-return *truncated the WAL
        // silently*, dropping the tombstones forever — the row in the old
        // segment stayed live and the new row landed in another segment, so
        // queries returned two copies of the same PK. This is the root cause
        // of the ghost-row bug that we worked around in v0.1.123..0.1.129
        // by persisting `deleted.bitmap` synchronously; with WAL replay
        // restored, those persistents become a fast path, not the only line
        // of defense.
        let ops = self.wal.replay()?;
        if ops.is_empty() {
            return Ok(());
        }
        info!(
            "Table '{}': replaying {} WAL operation(s) for crash recovery.",
            self.name,
            ops.len()
        );
        for op in ops {
            match op {
                WalOperation::Insert { id: _, data } => {
                    let row_data: HashMap<String, String> = serde_json::from_slice(&data)
                        .unwrap_or_default();
                    // Delegate to Table::insert: it is idempotent (compares
                    // existing on-disk row against `row_data` and either no-ops
                    // when equal or tombstone-and-replaces when different). This
                    // is what closes the ghost-row class of bugs: re-delivered
                    // binlog events AND re-played WAL entries both converge to
                    // a single live row per PK without leaving stale copies in
                    // older segments.
                    //
                    // The extra WAL append that `insert` performs is harmless:
                    // we truncate the WAL a few lines below this loop, so any
                    // ops re-appended during replay are wiped together with
                    // the originals.
                    self.insert(row_data)?;
                }
                WalOperation::Delete { id, seg_id, local_id } => {
                    let normalized_id: String = id.nfc().collect();
                    // The Delete WAL entry carries the row's location at the
                    // moment of the original delete(). But this replay may have
                    // ALREADY re-applied an Insert for the same PK (via the
                    // idempotent self.insert above), which means primary_index
                    // now points at a NEW (seg, local) for this id. We must
                    // tombstone *both*: the stored WAL location AND wherever
                    // primary_index currently points, then drop the id from
                    // primary_index. Otherwise the replayed Insert leaves a
                    // live row that the test (and binlog semantics) say should
                    // not exist.
                    let primary_loc = self.primary_index.remove(&normalized_id);
                    // Tombstone the WAL-recorded location.
                    if let Some(writer) = &mut self.active_segment {
                        if writer.segment.id == seg_id {
                            writer.segment.mark_deleted(local_id)?;
                            writer.segment.persist_deleted_bitmap()?;
                        } else if let Some(seg) = self
                            .immutable_segments
                            .iter_mut()
                            .find(|s| s.id == seg_id)
                        {
                            seg.mark_deleted(local_id)?;
                            seg.persist_deleted_bitmap()?;
                        }
                    }
                    // Additionally tombstone the replay-re-inserted copy, if
                    // primary_index pointed somewhere else.
                    if let Some((cur_seg, cur_local)) = primary_loc {
                        if (cur_seg, cur_local) != (seg_id, local_id) {
                            if let Some(writer) = &mut self.active_segment {
                                if writer.segment.id == cur_seg {
                                    writer.segment.mark_deleted(cur_local)?;
                                    writer.segment.persist_deleted_bitmap()?;
                                } else if let Some(seg) = self
                                    .immutable_segments
                                    .iter_mut()
                                    .find(|s| s.id == cur_seg)
                                {
                                    seg.mark_deleted(cur_local)?;
                                    seg.persist_deleted_bitmap()?;
                                }
                            }
                        }
                    }
                }
                WalOperation::Update { id, data, seg_id, local_id } => {
                    let mut row_data: HashMap<String, String> = serde_json::from_slice(&data)
                        .unwrap_or_default();
                    // Mark the old version dead at its known location.
                    if let Some(writer) = &mut self.active_segment {
                        if writer.segment.id == seg_id {
                            writer.segment.mark_deleted(local_id)?;
                            writer.segment.persist_deleted_bitmap()?;
                        } else if let Some(seg) =
                            self.immutable_segments.iter_mut().find(|s| s.id == seg_id)
                        {
                            seg.mark_deleted(local_id)?;
                            seg.persist_deleted_bitmap()?;
                        }
                    }
                    let pk_field = if self.manifest.primary_key.is_empty() {
                        "PK"
                    } else {
                        &self.manifest.primary_key
                    };
                    if let Some(pk_raw) = row_data.get(pk_field).cloned() {
                        let normalized: String = pk_raw.nfc().collect();
                        if normalized != pk_raw {
                            row_data.insert(pk_field.to_string(), normalized);
                        }
                    }
                    let pk_val = row_data.get(pk_field).cloned().unwrap_or_else(|| id.clone());
                    // Same idempotency check as Insert.
                    if let Some((cur_seg, cur_local)) = self.primary_index.get(&pk_val) {
                        if cur_seg != seg_id || cur_local != local_id {
                            // primary_index already points at the new row;
                            // the segment append from before the crash got
                            // through. Don't re-append.
                            continue;
                        }
                    }
                    if let Some(writer) = &mut self.active_segment {
                        let new_local_id = writer.segment.record_count as u32;
                        writer.append_record(&row_data)?;
                        self.primary_index.insert(&pk_val, (writer.segment.id, new_local_id));
                    }
                }
            }
        }
        self.save_primary_index()?;
        self.wal.flush_writes()?;
        self.wal.truncate()?;
        info!(
            "Table '{}': WAL replay complete; primary index saved.",
            self.name
        );
        Ok(())
    }

    pub fn reload_if_needed(&mut self) -> Result<()> {
        let manifest_path = self.base_path.join("manifest.json");
        if !manifest_path.exists() {
            return Ok(());
        }

        let file = fs::File::open(&manifest_path)?;
        let reader = std::io::BufReader::new(file);
        let new_manifest: Manifest = serde_json::from_reader(reader)?;

        if new_manifest.last_sequence_number > self.manifest.last_sequence_number 
           || (new_manifest.last_sequence_number == self.manifest.last_sequence_number && self.immutable_segments.is_empty() && !new_manifest.segments.is_empty()) 
        {
            info!("Table '{}' change detected (seq {} -> {}). Reloading segments...", 
                self.name, self.manifest.last_sequence_number, new_manifest.last_sequence_number);
            
            self.manifest = new_manifest;
            self.load_segments()?;
            self.load_primary_index()?;
            
            // Clean the active segment so that it is re-created if necessary
            self.active_segment = None;
            self.ensure_active_segment()?;
        }

        Ok(())
    }

    fn load_primary_index(&mut self) -> Result<()> {
        let idx_path = self.base_path.join("primary.idx");
        if idx_path.exists() {
            self.primary_index = primary_index_io::load_primary_index(&idx_path)?;
        }
        Ok(())
    }

    fn save_primary_index(&self) -> Result<()> {
        let idx_path = self.base_path.join("primary.idx");
        primary_index_io::save_primary_index(&idx_path, &self.primary_index)
    }

    fn load_segments(&mut self) -> Result<()> {
        let segments_dir = self.base_path.join("segments");
        let active_id = self.manifest.active_segment_id;
        self.immutable_segments = self.manifest.segments.par_iter()
            .filter(|seg_meta| seg_meta.id != active_id)
            .filter_map(|seg_meta| {
                let seg_path = segments_dir.join(format!("seg_{:04}", seg_meta.id));
                if seg_path.exists() { Segment::load(&seg_path, Some(seg_meta)).ok() }
                else { None }
            })
            .collect();
        self.immutable_segments.sort_by_key(|s| s.id);
        Ok(())
    }

    fn ensure_active_segment(&mut self) -> Result<()> {
        if self.active_segment.is_some() { return Ok(()); }
        let segments_dir = self.base_path.join("segments");
        let active_id = self.manifest.active_segment_id;
        let seg_path = segments_dir.join(format!("seg_{:04}", active_id));
        let segment = if seg_path.exists() {
            let meta = self.manifest.segments.iter().find(|s| s.id == active_id);
            match Segment::load(&seg_path, meta) {
                Ok(mut s) => { s.is_immutable = false; s }
                Err(e) => {
                    // Active segment is corrupt (e.g. leftover from an interrupted compact).
                    // Recover by starting a fresh empty segment at the same ID so that WAL
                    // replay on this open can still reconstruct any unflushed rows.
                    warn!(
                        "Table '{}': active segment {} failed to load ({}); creating fresh segment.",
                        self.name, active_id, e
                    );
                    let s = Segment::new(active_id, &segments_dir);
                    s.create_dirs()?;
                    s
                }
            }
        } else {
            let s = Segment::new(active_id, &segments_dir);
            s.create_dirs()?;
            s
        };
        self.active_segment = Some(SegmentWriter::new(segment));
        Ok(())
    }

    /// Removes every mirrored row (WAL tombstones + segment deletes). Used rarely after CDC checkpoint loss.
    ///
    /// Reads PK values from segments directly rather than from primary_index,
    /// so this works even after the index representation changes (e.g. hashed keys).
    pub fn delete_all_rows(&mut self) -> Result<()> {
        let pk_field = if self.manifest.primary_key.is_empty() {
            "PK".to_string()
        } else {
            self.manifest.primary_key.clone()
        };
        let pk_fields = vec![pk_field.clone()];

        // Collect PKs from all immutable segments.
        let mut all_pks: Vec<String> = Vec::new();
        for seg in &self.immutable_segments {
            let _ = seg.ensure_mmaps_batch(&pk_fields);
            for local_id in 0u32..seg.record_count as u32 {
                if seg.deleted_bitmap.contains(local_id) {
                    continue;
                }
                let vals = seg.get_row_values(local_id, &pk_fields)?;
                if let Some(pk) = vals.into_iter().next() {
                    all_pks.push(pk);
                }
            }
        }

        // Collect PKs from the active segment.
        if let Some(writer) = &self.active_segment {
            let seg = &writer.segment;
            let _ = seg.ensure_mmaps_batch(&pk_fields);
            for local_id in 0u32..seg.record_count as u32 {
                if seg.deleted_bitmap.contains(local_id) {
                    continue;
                }
                let vals = seg.get_row_values(local_id, &pk_fields)?;
                if let Some(pk) = vals.into_iter().next() {
                    all_pks.push(pk);
                }
            }
        }

        for pk in all_pks {
            self.delete(&pk)?;
        }
        self.flush_active_segment()
    }

    pub fn insert(&mut self, row_data: HashMap<String, String>) -> Result<()> {
        let pk_field_owned = if self.manifest.primary_key.is_empty() {
            "PK".to_string()
        } else {
            self.manifest.primary_key.clone()
        };
        // Normalize PK value to NFC before storing, so canonically equivalent
        // strings (e.g. NFC vs NFD) are stored identically and hash consistently.
        let mut row_data = row_data;
        if let Some(pk_val) = row_data.get(&pk_field_owned).cloned() {
            let normalized: String = pk_val.nfc().collect();
            if normalized != pk_val {
                row_data.insert(pk_field_owned.clone(), normalized);
            }
        }
        let id = row_data
            .get(&pk_field_owned)
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // ── Idempotency guard ───────────────────────────────────────────────
        // Binlog events can be re-delivered to the motor after a crash that
        // happened between applying the event (durable in segments + WAL) and
        // persisting `cdc_state.json` (periodic). Without this guard, the
        // re-delivery appends a duplicate physical row for the same PK,
        // primary_index gets re-pointed at the new copy, and the old copy
        // becomes a live ghost (no tombstone) — that is the root cause of the
        // ghost-row class of bugs.
        //
        // Three cases:
        //   1. PK not in primary_index → fall through, append normally.
        //   2. PK present and the on-disk row equals `row_data` →
        //      event already applied. No-op (no WAL append either: the WAL
        //      entry from the original apply is gone after truncate, and
        //      re-appending would just bloat the WAL).
        //   3. PK present but row differs (real UPDATE, OR pre-existing row
        //      was lost in a partial flush so it reads back as empty) →
        //      tombstone the old copy at its known location, then append the
        //      new one as a normal insert.
        if let Some((existing_seg, existing_local)) = self.primary_index.get(&id) {
            let existing = self.get_row_as_map(&id)?;
            if matches!(&existing, Some(map) if *map == row_data) {
                // Already applied. WAL is truncated periodically, so the
                // skipped WAL append here doesn't risk losing the row.
                return Ok(());
            }
            // Different content (or unreadable from disk): tombstone the old
            // location on the exact segment, then fall through and append.
            if let Some(writer) = &mut self.active_segment {
                if writer.segment.id == existing_seg {
                    writer.segment.mark_deleted(existing_local)?;
                    writer.segment.persist_deleted_bitmap()?;
                } else if let Some(seg) = self
                    .immutable_segments
                    .iter_mut()
                    .find(|s| s.id == existing_seg)
                {
                    seg.mark_deleted(existing_local)?;
                    seg.persist_deleted_bitmap()?;
                }
            }
        }

        let row_bytes = serde_json::to_vec(&row_data)?;
        let op = WalOperation::Insert { id: id.clone(), data: row_bytes };
        self.wal.append(&op)?;
        if let Some(writer) = &mut self.active_segment {
             let local_id = writer.segment.record_count as u32;
             writer.append_record(&row_data)?;
             self.primary_index.insert(&id, (writer.segment.id, local_id));
        }
        Ok(())
    }

    /// Full row as map by primary key value (for CDC merge on partial binlog UPDATE images).
    ///
    /// Takes `&mut self` so it can flush the active-segment BufWriters when the target row
    /// lives in the currently-dirty active segment.  Without this flush the mmap maps a
    /// shorter (stale) version of the file and every field reads back as an empty string,
    /// which then gets written verbatim by the UPDATE merge — losing all existing field values.
    pub fn get_row_as_map(&mut self, pk_val: &str) -> Result<Option<HashMap<String, String>>> {
        let Some((seg_id, local_id)) = self.primary_index.get(pk_val) else {
            return Ok(None);
        };
        let fields = &self.manifest.original_fields;
        if fields.is_empty() {
            return Ok(None);
        }
        // If the row sits in the active segment and the writer still has unflushed buffer
        // data, the mmap will map a shorter file and return empty strings for all fields.
        // Flush the BufWriters to the OS page cache before creating/using the mmap.
        if let Some(writer) = &mut self.active_segment {
            if writer.segment.id == seg_id && writer.dirty {
                writer.flush_buffers()?;
            }
        }
        let fields = self.manifest.original_fields.clone();
        let rows = self.get_rows_batch(&fields, &[(seg_id, local_id)])?;
        let Some(vals) = rows.first() else {
            return Ok(None);
        };
        let mut map = HashMap::with_capacity(fields.len());
        for (i, f) in fields.iter().enumerate() {
            map.insert(f.clone(), vals.get(i).cloned().unwrap_or_default());
        }

        // Collision verification: after reading the row, confirm the PK stored
        // in the row matches the requested PK (under canonical_bytes). A mismatch
        // means two distinct PKs hashed to the same u128 bucket — astronomically
        // unlikely with xxh3_128 but checked for correctness.
        let pk_field = if self.manifest.primary_key.is_empty() {
            "PK"
        } else {
            &self.manifest.primary_key
        };
        if let Some(actual_pk) = map.get(pk_field) {
            if canonical_bytes(actual_pk) != canonical_bytes(pk_val) {
                warn!(
                    "Hash collision detected in get_row_as_map: requested={:?}, actual={:?}",
                    pk_val, actual_pk
                );
                return Ok(None);
            }
        }

        Ok(Some(map))
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        if let Some((seg_id, local_id)) = self.primary_index.remove(id) {
            // WAL Delete now carries the physical location, so even if a SIGKILL
            // wins the race against the bitmap persist below, replay_wal can
            // still mark the right segment on the next startup. The in-line
            // persist that follows is a fast path, not the only safety net.
            let op = WalOperation::Delete {
                id: id.to_string(),
                seg_id,
                local_id,
            };
            self.wal.append(&op)?;
            if let Some(writer) = &mut self.active_segment {
                if writer.segment.id == seg_id {
                    writer.segment.mark_deleted(local_id)?;
                    writer.segment.persist_deleted_bitmap()?;
                } else if let Some(seg) = self.immutable_segments.iter_mut().find(|s| s.id == seg_id) {
                    seg.mark_deleted(local_id)?;
                    seg.persist_deleted_bitmap()?;
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
        // Each call uses a unique temp filename so that two concurrent callers (e.g. a discard
        // race that somehow slipped past the TableManager per-key lock) never share the same
        // manifest.tmp inode and interleave bytes.  The rename is always same-device (same dir)
        // so it is atomic on POSIX.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let tmp_name = format!("manifest.tmp.{}.{}", std::process::id(), seq);
        let manifest_path = self.base_path.join("manifest.json");
        let tmp_path = self.base_path.join(&tmp_name);
        {
            let file = fs::File::create(&tmp_path)?;
            serde_json::to_writer_pretty(&file, &self.manifest)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &manifest_path)
            .with_context(|| format!("rename {} → manifest.json", tmp_name))?;
        self.sync_base_dir()?;
        Ok(())
    }

    /// Flush only the WAL to OS page cache. Used during fast exit to ensure WAL
    /// entries survive a process crash without the expensive full segment flush.
    pub fn flush_wal_only(&mut self) -> Result<()> {
        self.wal.flush_writes()?;
        // Also persist every immutable segment's `deleted.bitmap`. Without this,
        // rows tombstoned by `delete()` since the last `flush_active_segment`
        // live only in memory — `fast_exit_flush` (DDL detect → process::exit,
        // engine halt requested, etc.) would lose them and they'd reappear on
        // restart as ghost duplicates against the freshly-inserted live row.
        self.persist_all_deleted_bitmaps()?;
        Ok(())
    }

    /// Number of rows written to the active (in-memory) segment so far.
    pub fn active_segment_row_count(&self) -> u64 {
        self.active_segment
            .as_ref()
            .map(|w| w.segment.record_count)
            .unwrap_or(0)
    }

    /// Flush the active segment's BufWriters and WAL to the OS page cache without rotating the segment.
    /// Cheaper than `flush_active_segment` (no bitmap serialisation, no segment rotation).
    /// Call after any CDC write when the full flush is batched, so concurrent mmap readers see
    /// the new row data immediately and WAL entries survive a process crash.
    pub fn flush_active_segment_buffers(&mut self) -> Result<()> {
        if let Some(writer) = &mut self.active_segment {
            writer.flush_buffers()?;
            // Persist per-column bitmaps for the active segment. Without
            // this, a crash before segment rotation leaves the active
            // segment on disk with rows but no `bitmaps_<col>.dat` files;
            // on restart, filtered queries miss those rows entirely (the
            // index-lag bug). SegmentWriter::new now rehydrates from disk
            // as a safety net, but persisting on every buffer flush keeps
            // the happy path fast (no scan-from-data on startup).
            writer.persist_bitmaps()?;
        }
        self.wal.flush_writes()?;
        self.flush_count_since_index_save += 1;
        let index_save_interval: u32 = std::env::var("BITTICE_INDEX_SAVE_INTERVAL")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(10);
        if self.flush_count_since_index_save >= index_save_interval {
            self.save_primary_index()?;
            self.wal.truncate()?;
            self.sync_base_dir()?;
            self.flush_count_since_index_save = 0;
        }
        Ok(())
    }

    pub fn flush_active_segment(&mut self) -> Result<()> {
        if let Some(mut writer) = self.active_segment.take() {
            writer.flush()?;
            // The throttle in `SegmentWriter::flush` (`flush_count % N == 0`)
            // is for mid-segment rewrites only — at rotation we MUST persist
            // the bitmaps so the sealed segment is queryable after a restart.
            // Without this, ~4 out of every 5 segments lose their per-column
            // value→row-set bitmaps; filtered queries skip those segments and
            // the mirror appears to drop rows.
            writer.persist_bitmaps()?;
            self.merge_exact_indexes_for_segment(writer.segment.id, &writer.bitmaps)?;
            let meta = writer.segment.to_meta();
            self.manifest.add_segment(meta);
            self.manifest.active_segment_id += 1;
            self.manifest.last_sequence_number += 1; 
            self.save_manifest()?;
            self.wal.flush_writes()?;
            self.flush_count_since_index_save += 1;
            let index_save_interval: u32 = std::env::var("BITTICE_INDEX_SAVE_INTERVAL")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(10);
            if self.flush_count_since_index_save >= index_save_interval {
                self.save_primary_index()?;
                self.wal.truncate()?;
                self.sync_base_dir()?;
                self.flush_count_since_index_save = 0;
            }
            let mut immutable_seg = writer.segment;
            immutable_seg.is_immutable = true;
            self.immutable_segments.push(immutable_seg);
            // No cache clear here: load_exact_index only caches the immutable view, and
            // merge_exact_indexes_for_segment above already updated each touched field's cache
            // entry to include the freshly-rotated segment.  Untouched fields stay valid.
        }
        self.persist_all_deleted_bitmaps()?;
        self.ensure_active_segment()?;
        Ok(())
    }

    fn persist_all_deleted_bitmaps(&self) -> Result<()> {
        for seg in &self.immutable_segments {
            if seg.has_deletions() {
                seg.persist_deleted_bitmap()?;
            }
        }
        Ok(())
    }

    fn sync_base_dir(&self) -> Result<()> {
        let dir_file = fs::File::open(&self.base_path)?;
        dir_file.sync_all()?;
        Ok(())
    }

    pub fn warm_up(&self, fields: &[String]) -> Result<()> {
        for field in fields {
            let _ = self.load_exact_index(field);
        }
        self.immutable_segments.par_iter().for_each(|seg| {
            let _ = seg.prefetch_fields(fields);
            let _ = seg.warm_up_indices(fields);
        });
        if let Some(writer) = &self.active_segment {
            let _ = writer.segment.prefetch_fields(fields);
            let _ = writer.segment.warm_up_indices(fields);
        }
        Ok(())
    }

    pub fn get_rows_batch(&self, fields: &[String], ids: &[(u64, u32)]) -> Result<Vec<Vec<String>>> {
        let mut segments_map = HashMap::new();
        for s in &self.immutable_segments { segments_map.insert(s.id, s); }
        if let Some(writer) = &self.active_segment { segments_map.insert(writer.segment.id, &writer.segment); }
        let needed_seg_ids: std::collections::HashSet<u64> = ids.iter().map(|(id, _)| *id).collect();
        let segment_field_mmaps: HashMap<u64, Vec<Option<Arc<(memmap2::Mmap, memmap2::Mmap)>>>> = segments_map.par_iter()
            .filter(|(id, _)| needed_seg_ids.contains(id))
            .map(|(id, segment)| {
                let _ = segment.ensure_mmaps_batch(fields);
                let mmaps = segment.get_mmaps_batch(fields);
                (*id, mmaps)
            })
            .collect();
        const CHUNK_SIZE: usize = 512;
        let rows: Vec<Vec<String>> = ids.par_chunks(CHUNK_SIZE)
            .flat_map(|chunk| {
                let columns_data: Vec<Vec<String>> = fields.par_iter().enumerate()
                    .map(|(field_idx, _)| {
                        let mut col_values = Vec::with_capacity(chunk.len());
                        let mut last_seg_id = u64::MAX;
                        let mut last_mmap: Option<&(memmap2::Mmap, memmap2::Mmap)> = None;
                        for (seg_id, local_id) in chunk {
                            if *seg_id != last_seg_id {
                                last_seg_id = *seg_id;
                                last_mmap = None;
                                if let Some(mmaps) = segment_field_mmaps.get(seg_id) {
                                    if let Some(mmap_pair) = &mmaps[field_idx] { last_mmap = Some(&**mmap_pair); }
                                }
                            }
                            let val = if let Some(mmap_pair) = last_mmap {
                                Segment::read_value_from_mmap(mmap_pair, *local_id)
                            } else { String::new() };
                            col_values.push(val);
                        }
                        col_values
                    })
                    .collect();
                let mut chunk_rows = Vec::with_capacity(chunk.len());
                let mut col_iters: Vec<_> = columns_data.into_iter().map(|c| c.into_iter()).collect();
                for _ in 0..chunk.len() {
                    let mut row = Vec::with_capacity(fields.len());
                    for iter in &mut col_iters { row.push(iter.next().unwrap()); }
                    chunk_rows.push(row);
                }
                chunk_rows
            })
            .collect();
        Ok(rows)
    }

    pub fn search(
        &self,
        fields: &[String],
        filters: &[Filter],
        filters_op: &LogicalOp,
        aggregations: &[serde_json::Value],
        order_by: &[OrderBy],
        limit: usize,
        offset: usize,
        auth_context: Option<&crate::core::types::AuthContext>,
    ) -> Result<QueryResult> {
        // Hard ceiling to prevent unbounded scans; matches the HTTP layer's
        // HARD_MAX_LIMIT (see src/server/mod.rs). The previous cap of 100
        // forced diagnostic saved ops to paginate even for small fleets.
        let limit = limit.min(10_000);
        let start_time = std::time::Instant::now();

        // Security Filter Injection (Row-Level Security)
        let mut final_filters = filters.to_vec();
        let mut final_filters_op = filters_op.clone();

        if let Some(ctx) = auth_context {
            let filter_col = ctx.filter_col.clone();
            
            // If the current table is the auth table itself, we don't inject the filter 
            // to avoid infinite recursion or blocking auth lookups.
            // Using a simple check against common auth table names or environment
            let auth_table = std::env::var("BITTICE_AUTH_TABLE").unwrap_or_else(|_| "auth_sessions".to_string());
            
            if self.name != auth_table && self.name != "Users" && self.name != "users" {
                final_filters.push(Filter {
                    field: filter_col,
                    op: crate::core::types::ComparisonOp::Eq,
                    value: ctx.user_id.clone(),
                    value_to: None,
                    field_type: None,
                    value_options: vec![],
                });
                
                final_filters_op = LogicalOp::And; 
            }
        }

        let mut segment_tasks: Vec<&Segment> = self.immutable_segments.iter().collect();
        if let Some(writer) = &self.active_segment { segment_tasks.push(&writer.segment); }
        let filter_start = std::time::Instant::now();
        let pk_field = if self.manifest.primary_key.is_empty() { "PK" } else { &self.manifest.primary_key };

        // Normalize PK filter values to NFC so that canonically equivalent
        // strings (e.g. "café" NFC vs NFD) match correctly. Without this,
        // a query with NFD form would fail candidate_matches_filters even
        // though the row was stored with NFC form (same canonical_bytes hash).
        for filter in final_filters.iter_mut() {
            if filter.field == pk_field {
                filter.value = filter.value.nfc().collect();
                if let Some(ref mut v) = filter.value_to {
                    *v = v.nfc().collect();
                }
                for opt in filter.value_options.iter_mut() {
                    *opt = opt.nfc().collect();
                }
            }
        }

        let pk_filter = final_filters.iter().find(|f| f.field == pk_field && f.op == crate::core::types::ComparisonOp::Eq);
        let pk_in_filter = final_filters.iter().find(|f| f.field == pk_field && f.op == crate::core::types::ComparisonOp::In);
        let segment_matches = if let Some(f) = pk_filter {
            let use_index = final_filters.len() == 1 || final_filters_op == LogicalOp::And;
            if use_index {
                if let Some((seg_id, local_id)) = self.primary_index.get(&f.value) {
                    let candidate = (seg_id, local_id);
                    if self.candidate_matches_filters(candidate, &final_filters, &final_filters_op)? {
                        let mut bm = RoaringBitmap::new();
                        bm.insert(local_id);
                        vec![(seg_id, bm)]
                    } else {
                        vec![]
                    }
                } else { vec![] }
            } else { self.scan_segments_parallel(&segment_tasks, &final_filters, &final_filters_op)? }
        } else if let Some(f) = pk_in_filter {
            let use_index = final_filters.len() == 1 || final_filters_op == LogicalOp::And;
            if use_index {
                // PK IN: resolve directly through `primary_index` instead of the exact_index.
                // Since the PK exact_index is intentionally not built (see should_skip_exact_index),
                // every PK IN lookup would otherwise fall back to a full segment scan.  Doing
                // N hash lookups against `primary_index` is O(N) and avoids touching segments.
                let values = crate::core::types::list_values(&f.value, &f.value_options);
                let mut by_seg: HashMap<u64, RoaringBitmap> = HashMap::new();
                for value in &values {
                    if let Some((seg_id, local_id)) = self.primary_index.get(value) {
                        by_seg.entry(seg_id).or_default().insert(local_id);
                    }
                }
                let mut matches: Vec<(u64, RoaringBitmap)> =
                    by_seg.into_iter().filter(|(_, bm)| !bm.is_empty()).collect();
                if final_filters.len() > 1 {
                    // Other filters present: re-validate each candidate against them.
                    matches.retain(|(seg_id, bm)| {
                        bm.iter().any(|local_id| {
                            self.candidate_matches_filters(
                                (*seg_id, local_id),
                                &final_filters,
                                &final_filters_op,
                            ).unwrap_or(false)
                        })
                    });
                }
                matches
            } else {
                self.scan_segments_parallel(&segment_tasks, &final_filters, &final_filters_op)?
            }
        } else if final_filters_op == LogicalOp::And {
            if let Some(exact_matches) = self.exact_matches_for_eq_filters(&final_filters, pk_field)? {
                let remaining_filters: Vec<Filter> = final_filters.iter()
                    .filter(|filter| !(filter.op == crate::core::types::ComparisonOp::Eq && filter.field != pk_field)
                        && !(filter.op == crate::core::types::ComparisonOp::In))
                    .cloned()
                    .collect();
                if remaining_filters.is_empty() {
                    exact_matches
                } else {
                    let scanned = self.scan_segments_parallel(&segment_tasks, &remaining_filters, &LogicalOp::And)?;
                    intersect_segment_matches(exact_matches, scanned)
                }
            } else {
                self.scan_segments_parallel(&segment_tasks, &final_filters, &final_filters_op)?
            }
        } else { self.scan_segments_parallel(&segment_tasks, &final_filters, &final_filters_op)? };
        let mut total_found = 0;
        for (seg_id, bitmap) in &segment_matches { 
            total_found += bitmap.len(); 
            debug!("Search: Segment {} matched {} rows", seg_id, bitmap.len());
        }
        let filter_elapsed = filter_start.elapsed().as_micros();
        let mut aggregation_results = Vec::new();
        let agg_start = std::time::Instant::now();
        if !aggregations.is_empty() {
            for agg in aggregations {
                let mut agg_headers = Vec::new();
                let mut agg_rows = Vec::new();
                if let Some(obj) = agg.as_object() {
                    let agg_type = obj.keys().next().unwrap();
                    let params = obj.get(agg_type).unwrap();
                    if agg_type == "Count" { aggregation_results.push(crate::core::types::AggregationResult { headers: vec![], rows: vec![], summary: Some(total_found as f64) }); continue; }
                    else if agg_type == "GroupBy" || agg_type == "TopN" {
                        let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("?");
                        let mut global_counts: HashMap<String, u64> = HashMap::new();
                        for (seg_id, bitmap) in &segment_matches {
                            let seg_counts = if let Some(writer) = &self.active_segment {
                                if writer.segment.id == *seg_id { writer.get_counts(field, bitmap)? }
                                else { 
                                    let s = self.immutable_segments.iter().find(|s| s.id == *seg_id).unwrap();
                                    s.get_counts_thread_safe(field, bitmap)? 
                                }
                            } else { 
                                let s = self.immutable_segments.iter().find(|s| s.id == *seg_id).unwrap();
                                s.get_counts_thread_safe(field, bitmap)? 
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
                            results.sort_by(|a, b| { let cmp = a.0.cmp(&b.0); if direction == SortDirection::Desc { cmp.reverse() } else { cmp } });
                        }
                        let total_count: u64 = results.iter().map(|(_, c)| c).sum();
                        agg_headers = vec![field.to_string(), "count".to_string()];
                        agg_rows = results.into_iter().map(|(v, c)| vec![v, c.to_string()]).collect();
                        aggregation_results.push(crate::core::types::AggregationResult { headers: agg_headers, rows: agg_rows, summary: Some(total_count as f64) });
                        continue;
                    } else if agg_type == "Sum" {
                        let group_by = params.get("group_by").and_then(|v| v.as_str());
                        let field_val = params.get("field").and_then(|v| v.as_str()).filter(|&s| s != "?");
                        let expr_val = params.get("expression").and_then(|v| v.as_str());

                        let (field_name_opt, expression_str) = match (group_by, field_val, expr_val) {
                            (Some(gb), Some(f), None) => (Some(gb), f),
                            (Some(gb), _, Some(e)) => (Some(gb), e),
                            (None, Some(f), Some(e)) => (Some(f), e),
                            (None, Some(f), None) => (Some(f), "0"),
                            (None, None, Some(e)) => (None, e),
                            _ => (None, "0")
                        };

                        if let Ok(expr) = crate::core::expression::parse_expression(expression_str) {
                            let required_fields = crate::core::expression::extract_fields(&expr);
                            if let Some(field_name) = field_name_opt {
                                let use_par = total_found > 500;
                                let segment_group_results: Vec<HashMap<String, f64>> = if use_par { segment_matches.par_iter().map(|(seg_id, bitmap)| self.process_seg_sum_grouped(*seg_id, bitmap, field_name, &required_fields, &expr)).collect() }
                                else { segment_matches.iter().map(|(seg_id, bitmap)| self.process_seg_sum_grouped(*seg_id, bitmap, field_name, &required_fields, &expr)).collect() };
                                let mut global_group_sums = HashMap::new();
                                let mut total_sum = 0.0;
                                for seg_map in segment_group_results { for (k, v) in seg_map { total_sum += v; *global_group_sums.entry(k).or_insert(0.0) += v; } }
                                let mut results: Vec<(String, f64)> = global_group_sums.into_iter().collect();
                                results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
                                
                                let header_sum = if expression_str.chars().all(|c| c.is_alphanumeric() || c == '_') {
                                    format!("{}_sum", expression_str)
                                } else { "sum".to_string() };

                                agg_headers = vec![field_name.to_string(), header_sum];
                                agg_rows = results.into_iter().map(|(v, s)| vec![v, format!("{:.2}", s)]).collect();
                                aggregation_results.push(crate::core::types::AggregationResult { headers: agg_headers, rows: agg_rows, summary: Some(total_sum) });
                                continue; 
                            } else {
                                let use_par = total_found > 500;
                                let total_sum: f64 = if use_par { segment_matches.par_iter().map(|(seg_id, bitmap)| self.process_seg_sum_global(*seg_id, bitmap, &required_fields, &expr)).sum() }
                                else { segment_matches.iter().map(|(seg_id, bitmap)| self.process_seg_sum_global(*seg_id, bitmap, &required_fields, &expr)).sum() };
                                aggregation_results.push(crate::core::types::AggregationResult { headers: vec![], rows: vec![], summary: Some(total_sum) });
                                continue;
                            }
                        } else { agg_headers = vec!["error".to_string()]; agg_rows = vec![vec!["Invalid expression".to_string()]]; }
                    }
                }
                aggregation_results.push(crate::core::types::AggregationResult { headers: agg_headers, rows: agg_rows, summary: None });
            }
            // EARLY RETURN: If there are aggregations and NO specific fields were requested, return only the aggregates.
            if fields.is_empty() {
                let total_elapsed = start_time.elapsed().as_micros();
                let agg_elapsed = agg_start.elapsed().as_micros();
                let debug = format!("Filter: {}ms, Agg: {}ms, Segments: {}, Mode: AggOnly", filter_elapsed / 1000, agg_elapsed / 1000, segment_tasks.len());
                return Ok(QueryResult { 
                    headers: vec![], 
                    rows: vec![], 
                    row_ids: None, 
                    total_found: total_found as usize, 
                    execution_time_micros: total_elapsed, 
                    debug_info: Some(debug), 
                    aggregations: Some(aggregation_results) 
                });
            }
        }
        if limit == 0 {
            let total_elapsed = start_time.elapsed().as_micros();
            let debug = format!("Filter: {}ms, Limit: 0, Segments: {}", filter_elapsed / 1000, segment_tasks.len());
            return Ok(QueryResult { headers: fields.to_vec(), rows: vec![], row_ids: None, total_found: total_found as usize, execution_time_micros: total_elapsed, debug_info: Some(debug), aggregations: if aggregation_results.is_empty() { None } else { Some(aggregation_results) } });
        }
        let sort_start = std::time::Instant::now();
        let mut final_ids: Vec<(u64, u32)>;
        if !order_by.is_empty() {
            let sort_field = &order_by[0].field;
            let direction = order_by[0].direction;
            let limit_n = if limit == 0 { 100 } else { limit };
            let effective_limit = offset + limit_n; 
            let mut all_ids = Vec::with_capacity(total_found as usize);
            for (seg_id, bitmap) in &segment_matches { for id in bitmap { all_ids.push((*seg_id, id)); } }
            let mut mmap_refs = HashMap::new();
            for (seg_id, _) in &segment_matches {
                 let segment = if let Some(writer) = &self.active_segment { if writer.segment.id == *seg_id { Some(&writer.segment) } else { self.immutable_segments.iter().find(|s| s.id == *seg_id) } }
                 else { self.immutable_segments.iter().find(|s| s.id == *seg_id) };
                if let Some(s) = segment { if let Ok(m) = s.get_mmap_pair(sort_field) { mmap_refs.insert(*seg_id, m); } }
            }
            let top_k_items = all_ids.par_chunks(4096)
                .fold(|| (Vec::with_capacity(effective_limit * 4), None::<SortKey>), |mut state, chunk| {
                    let acc = &mut state.0; let threshold = &mut state.1; let buffer_limit = effective_limit * 4;
                    for (seg_id, local_id) in chunk {
                        let ref_key = if let Some(mmap_pair) = mmap_refs.get(seg_id) {
                            let dat = &mmap_pair.0; let off = &mmap_pair.1; let start_idx = (*local_id as usize) << 3;
                            if start_idx + 8 <= off.len() {
                                let start_pos = u64::from_le_bytes(off[start_idx..start_idx+8].try_into().unwrap()) as usize;
                                if start_pos + 8 <= dat.len() {
                                    let len = u64::from_le_bytes(dat[start_pos..start_pos+8].try_into().unwrap()) as usize;
                                    if let Ok(s) = std::str::from_utf8(&dat[start_pos+8..start_pos+8+len]) {
                                        if !s.is_empty() && s.as_bytes()[0].is_ascii_digit() { if let Ok(n) = s.parse::<f64>() { RefSortKey::Num(n) } else { RefSortKey::Str(s) } }
                                        else { RefSortKey::Str(s) }
                                    } else { RefSortKey::None }
                                } else { RefSortKey::None }
                            } else { RefSortKey::None }
                        } else { RefSortKey::None };
                        if let Some(thresh) = threshold {
                             let cmp = compare_ref_owned(&ref_key, thresh);
                             let skip = if direction == SortDirection::Desc { cmp != Ordering::Greater } else { cmp != Ordering::Less };
                             if skip { continue; }
                        }
                        let key = match ref_key { RefSortKey::Num(n) => SortKey::Num(n), RefSortKey::Str(s) => SortKey::Str(s.to_string()), RefSortKey::None => SortKey::None, };
                        acc.push(HeapItem { key, seg_id: *seg_id, local_id: *local_id });
                        if acc.len() >= buffer_limit {
                             acc.sort_unstable_by(|a, b| { let cmp = a.cmp(b); if direction == SortDirection::Desc { cmp.reverse() } else { cmp } });
                             if acc.len() > effective_limit { acc.truncate(effective_limit); if let Some(last) = acc.last() { *threshold = Some(last.key.clone()); } }
                        }
                    }
                    state
                })
                .map(|(acc, _)| acc)
                .reduce(|| Vec::with_capacity(effective_limit), |mut a, b| {
                    a.extend(b); a.sort_unstable_by(|x, y| { let cmp = x.cmp(y); if direction == SortDirection::Desc { cmp.reverse() } else { cmp } });
                    if a.len() > effective_limit { a.truncate(effective_limit); }
                    a
                });
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
        let fetch_start = std::time::Instant::now();
        let rows = self.get_rows_batch(fields, &final_ids)?;
        let fetch_elapsed = fetch_start.elapsed().as_micros();
        let total_elapsed = start_time.elapsed().as_micros();
        let debug = format!("Filter: {}ms, Sort: {}ms, Fetch: {}ms, Segments: {}, FetchMode: Vectorized", filter_elapsed / 1000, sort_elapsed / 1000, fetch_elapsed / 1000, segment_tasks.len());
        let final_rows = if rows.len() > 1000 { Vec::new() } else { rows };
        Ok(QueryResult { headers: fields.to_vec(), rows: final_rows, row_ids: Some(final_ids), total_found: total_found as usize, execution_time_micros: total_elapsed, debug_info: Some(debug), aggregations: if aggregation_results.is_empty() { None } else { Some(aggregation_results) } })
    }

    fn candidate_matches_filters(
        &self,
        candidate: (u64, u32),
        filters: &[Filter],
        filters_op: &LogicalOp,
    ) -> Result<bool> {
        let valid_filters: Vec<&Filter> = filters.iter().filter(|f| f.field != "?" && f.value != "?").collect();
        if valid_filters.is_empty() {
            return Ok(true);
        }

        let mut field_positions: HashMap<String, usize> = HashMap::new();
        let mut filter_fields = Vec::new();
        for filter in &valid_filters {
            if !field_positions.contains_key(&filter.field) {
                field_positions.insert(filter.field.clone(), filter_fields.len());
                filter_fields.push(filter.field.clone());
            }
        }

        let candidate_rows = self.get_rows_batch(&filter_fields, &[candidate])?;
        let Some(candidate_row) = candidate_rows.first() else {
            return Ok(false);
        };

        let mut matches = valid_filters.iter().map(|filter| {
            let actual = field_positions
                .get(&filter.field)
                .and_then(|index| candidate_row.get(*index))
                .map(String::as_str)
                .unwrap_or("");
            compare_filter_value(
                actual,
                filter.op,
                &filter.value,
                filter.value_to.as_deref(),
                &filter.value_options,
                filter.field_type,
            )
        });

        Ok(match filters_op {
            LogicalOp::And => matches.all(|matched| matched),
            LogicalOp::Or => matches.any(|matched| matched),
        })
    }

    fn exact_index_dir(&self) -> PathBuf {
        self.base_path.join("secondary_exact")
    }

    fn exact_index_path(&self, field: &str) -> PathBuf {
        self.exact_index_dir().join(format!("exact_{}.idx", field))
    }

    /// Returns true when this column's merged exact_index should NOT be built or persisted.
    ///
    /// The PK is always skipped because `primary_index` already serves PK Eq lookups in O(1)
    /// — the merged `exact_<PK>.idx` would just be a much heavier duplicate (333 MB on
    /// large PKs).  Operators can list extra fields (e.g. `FechaCreacion`, `FechaActualizacion`)
    /// in `BITTICE_SKIP_EXACT_INDEX_FIELDS` (comma-separated) when those fields are only
    /// queried as ranges, not Eq.
    fn should_skip_exact_index(&self, field: &str) -> bool {
        if !self.manifest.primary_key.is_empty() && field == self.manifest.primary_key {
            return true;
        }
        if let Ok(list) = std::env::var("BITTICE_SKIP_EXACT_INDEX_FIELDS") {
            for entry in list.split(',') {
                if entry.trim() == field {
                    return true;
                }
            }
        }
        false
    }

    /// Load the exact index for `field`, covering only **immutable** segments.
    ///
    /// The active segment is intentionally NOT merged here so the cached value is stable across
    /// CDC writes and only changes when a segment rotates (via `merge_exact_indexes_for_segment`).
    /// Callers that need the active-segment view (e.g. `exact_matches_for_field_value`) merge
    /// `writer.bitmaps[field]` themselves, which is cheap (a single HashMap lookup, no disk I/O).
    fn load_exact_index(&self, field: &str) -> Result<Arc<StdRwLock<ExactIndex>>> {
        if let Some(cached) = self.exact_index_cache.read().unwrap().get(field) {
            return Ok(cached.clone());
        }

        // Short-circuit for skipped fields (PK + operator-listed columns) — return an
        // empty index without touching disk.  Callers (e.g. exact_matches_for_field_value)
        // treat empty as "no exact-index match" and fall back to the proper code path
        // (primary_index for PK Eq/IN, scan_segments_parallel for everything else).
        if self.should_skip_exact_index(field) {
            return Ok(Arc::new(StdRwLock::new(ExactIndex::new())));
        }

        let t = std::time::Instant::now();
        let path = self.exact_index_path(field);
        let mut index: ExactIndex = if path.exists() {
            ExactIndex::open(&path)?
        } else {
            let mut built = self.build_exact_index(field)?;
            built.bind_path(path.clone());
            built
        };

        // Disk index is persisted only on close()/compact(); any segment rotated
        // since the last persist lives in memory and is lost on restart. After
        // an unclean shutdown the disk index points at older segments while
        // newer segments hold the live rows — lookups follow the index to the
        // stale segment, see the tombstone in deleted.bitmap, and return 0.
        // Patch missing segments by reading their bitmaps_<field>.dat directly.
        let known = index.segment_ids();
        let mut patched_seg_count = 0usize;
        for segment in &self.immutable_segments {
            if known.contains(&segment.id) {
                continue;
            }
            let bitmap_path = segment.path.join(format!("bitmaps_{}.dat", field));
            if !bitmap_path.exists() {
                continue;
            }
            let file = match fs::File::open(&bitmap_path) {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        "load_exact_index: open bitmaps file {:?} failed: {} (skipping)",
                        bitmap_path, e
                    );
                    continue;
                }
            };
            let reader = BufReader::new(file);
            let bitmaps: HashMap<String, RoaringBitmap> =
                match bincode::deserialize_from(reader) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(
                            "load_exact_index: decode bitmaps file {:?} failed: {} (skipping)",
                            bitmap_path, e
                        );
                        continue;
                    }
                };
            let non_empty: HashMap<String, RoaringBitmap> = bitmaps
                .into_iter()
                .filter(|(_, b)| !b.is_empty())
                .collect();
            if !non_empty.is_empty() {
                index.merge_segment(segment.id, non_empty);
                patched_seg_count += 1;
            }
        }
        if patched_seg_count > 0 {
            warn!(
                "load_exact_index PATCHED table='{}' field='{}' — merged {} segment(s) \
                 missing from on-disk index (likely a previous unclean shutdown); \
                 persisting refreshed index to disk.",
                self.name, field, patched_seg_count
            );
            // One-time write at startup, not on the hot path — safe to do disk I/O.
            // Subsequent restarts skip the rebuild.
            if let Err(e) = index.save(Some(&path)) {
                warn!(
                    "load_exact_index: persist patched index for field='{}' failed: {} \
                     (in-memory index is still correct; next compact will rewrite)",
                    field, e
                );
            }
        }

        let load_ms = t.elapsed().as_secs_f64() * 1000.0;
        if load_ms > 50.0 {
            warn!(
                "load_exact_index SLOW table='{}' field='{}' took {:.1}ms entries={}",
                self.name, field, load_ms, index.len()
            );
        }

        let cached = Arc::new(StdRwLock::new(index));
        let mut cache = self.exact_index_cache.write().unwrap();
        // Evict one arbitrary entry when the cache is at capacity to bound RAM usage.
        // Evicted entries are rebuilt from disk on the next access.
        let max_fields: usize = std::env::var("BITTICE_EXACT_INDEX_CACHE_FIELDS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(32);
        if cache.len() >= max_fields && !cache.contains_key(field) {
            if let Some(evict_key) = cache.keys().next().cloned() {
                cache.remove(&evict_key);
            }
        }
        let entry = cache.entry(field.to_string()).or_insert_with(|| cached.clone());
        Ok(entry.clone())
    }

    fn build_exact_index(&self, field: &str) -> Result<ExactIndex> {
        let mut index = ExactIndex::new();
        for segment in &self.immutable_segments {
            let bitmap_path = segment.path.join(format!("bitmaps_{}.dat", field));
            if !bitmap_path.exists() {
                continue;
            }
            let file = fs::File::open(bitmap_path)?;
            let reader = BufReader::new(file);
            let bitmaps: HashMap<String, RoaringBitmap> = bincode::deserialize_from(reader)?;
            let non_empty: HashMap<String, RoaringBitmap> = bitmaps
                .into_iter()
                .filter(|(_, b)| !b.is_empty())
                .collect();
            index.merge_segment(segment.id, non_empty);
        }
        if let Some(writer) = &self.active_segment {
            if let Some(field_bitmaps) = writer.bitmaps.get(field) {
                let mut live_bitmaps: HashMap<String, RoaringBitmap> = HashMap::new();
                for (value, bitmap) in field_bitmaps {
                    if !bitmap.is_empty() {
                        let mut live_bitmap = bitmap.clone();
                        if !writer.segment.deleted_bitmap.is_empty() {
                            live_bitmap -= &writer.segment.deleted_bitmap;
                        }
                        if !live_bitmap.is_empty() {
                            live_bitmaps.insert(value.clone(), live_bitmap);
                        }
                    }
                }
                index.merge_segment(writer.segment.id, live_bitmaps);
            }
        }
        Ok(index)
    }

    fn save_exact_index(&self, field: &str, index: &mut ExactIndex) -> Result<()> {
        let path = self.exact_index_path(field);
        index.save(Some(&path))
    }

    /// Rewrite every on-disk exact index file removing entries that reference any of the
    /// `removed_segment_ids`.  Called after `compact()` deletes old segments — without this,
    /// the merged index keeps growing forever and queries pay extra work for stale entries
    /// that resolve to deleted segments.
    fn purge_exact_indexes_for_removed_segments(&self, removed_segment_ids: &[u64]) -> Result<()> {
        if removed_segment_ids.is_empty() {
            return Ok(());
        }
        let removed: std::collections::HashSet<u64> = removed_segment_ids.iter().copied().collect();
        let dir = self.exact_index_dir();
        if !dir.exists() {
            return Ok(());
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = match file_name.to_str() {
                Some(n) => n,
                None => continue,
            };
            if !name.starts_with("exact_") || !name.ends_with(".idx") {
                continue;
            }
            let field = name.trim_start_matches("exact_").trim_end_matches(".idx").to_string();
            let path = entry.path();
            let mut index: ExactIndex = match ExactIndex::open(&path) {
                Ok(i) => i,
                Err(_) => continue,
            };
            if index.purge_segments(&removed) {
                self.save_exact_index(&field, &mut index)?;
            }
        }
        Ok(())
    }

    /// Merge a freshly-rotated segment's bitmaps into the in-memory exact_index cache only.
    ///
    /// Disk persistence is intentionally skipped here because this runs on the CDC hot path
    /// while the Table write lock is held — rewriting hundreds of MB per high-cardinality
    /// field per rotation freezes every concurrent reader for seconds.  The per-segment
    /// `bitmaps_<field>.dat` files are still persisted (by `SegmentWriter::flush`), so the
    /// index can be rebuilt from disk on next `Table::open` if the in-memory cache is cold.
    ///
    /// Compaction (`compact()` and `purge_exact_indexes_for_removed_segments`) and explicit
    /// `persist_all_exact_indexes` calls flush the cache to disk.
    /// Merge a freshly-rotated segment's bitmaps into the in-memory exact_index cache only.
    ///
    /// Disk persistence is intentionally skipped here because this runs on the CDC hot path
    /// while the Table write lock is held — rewriting hundreds of MB per high-cardinality
    /// field per rotation freezes every concurrent reader for seconds.  The per-segment
    /// `bitmaps_<field>.dat` files are still persisted (by `SegmentWriter::flush`), and
    /// `close()`/`compact()` flush the cache to the on-disk merged index.
    fn merge_exact_indexes_for_segment(
        &self,
        segment_id: u64,
        segment_bitmaps: &HashMap<String, HashMap<String, RoaringBitmap>>,
    ) -> Result<()> {
        for (field, values) in segment_bitmaps {
            if self.should_skip_exact_index(field) {
                continue;
            }

            let existing = self.exact_index_cache.read().unwrap().get(field).cloned();
            let arc = if let Some(arc) = existing {
                arc
            } else {
                // Cache miss: load from disk (read is cheap) so the merged result also covers
                // segments persisted by previous sessions.  We still avoid writing back here.
                let path = self.exact_index_path(field);
                let initial: ExactIndex = if path.exists() {
                    ExactIndex::open(&path).unwrap_or_default()
                } else {
                    let mut idx = ExactIndex::new();
                    idx.bind_path(path);
                    idx
                };
                let new_arc = Arc::new(StdRwLock::new(initial));
                let mut cache = self.exact_index_cache.write().unwrap();
                cache.entry(field.clone()).or_insert_with(|| new_arc.clone()).clone()
            };

            // Append in place — O(values touched), not O(index size).  Concurrent readers
            // briefly block on the per-field write lock instead of the whole-cache lock.
            let mut guard = arc.write().unwrap();
            let non_empty: HashMap<String, RoaringBitmap> = values
                .iter()
                .filter(|(_, b)| !b.is_empty())
                .map(|(v, b)| (v.clone(), b.clone()))
                .collect();
            guard.merge_segment(segment_id, non_empty);
        }

        Ok(())
    }

    /// Write every in-memory exact_index entry to disk.  Called by `compact()` and `close()`
    /// so that crash recovery sees an up-to-date index without paying disk I/O on every CDC
    /// segment rotation.
    fn persist_all_exact_indexes(&self) -> Result<()> {
        let snapshot: Vec<(String, Arc<StdRwLock<ExactIndex>>)> = {
            let cache = self.exact_index_cache.read().unwrap();
            cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        for (field, arc) in snapshot {
            let mut guard = arc.write().unwrap();
            self.save_exact_index(&field, &mut guard)?;
        }
        Ok(())
    }

    fn exact_matches_for_eq_filters(&self, filters: &[Filter], _pk_field: &str) -> Result<Option<Vec<(u64, RoaringBitmap)>>> {
        let eq_filters: Vec<&Filter> = filters.iter()
            .filter(|filter| {
                (filter.op == crate::core::types::ComparisonOp::Eq
                 || filter.op == crate::core::types::ComparisonOp::In)
                    && filter.field != "?"
                    && filter.value != "?"
            })
            .collect();

        if eq_filters.is_empty() {
            return Ok(None);
        }

        let mut exact_matches: Option<Vec<(u64, RoaringBitmap)>> = None;

        for filter in eq_filters {
            let current_matches = if filter.op == crate::core::types::ComparisonOp::In {
                self.exact_matches_for_in_filter(filter)?
            } else {
                self.exact_matches_for_field_value(filter)?
            };
            let Some(current_matches) = current_matches else {
                if filter.op == crate::core::types::ComparisonOp::In {
                    break;
                }
                return Ok(None);
            };

            exact_matches = Some(match exact_matches {
                None => current_matches,
                Some(existing) => intersect_segment_matches(existing, current_matches),
            });

            if exact_matches.as_ref().is_some_and(|matches| matches.is_empty()) {
                break;
            }
        }

        Ok(exact_matches)
    }

    fn exact_matches_for_in_filter(&self, filter: &Filter) -> Result<Option<Vec<(u64, RoaringBitmap)>>> {
        let values = crate::core::types::list_values(&filter.value, &filter.value_options);
        let mut found_any = false;
        let mut combined: HashMap<u64, RoaringBitmap> = HashMap::new();

        for value in &values {
            let single = Filter {
                field: filter.field.clone(),
                op: crate::core::types::ComparisonOp::Eq,
                value: value.clone(),
                value_to: None,
                value_options: vec![],
                field_type: filter.field_type,
            };
            if let Some(matches) = self.exact_matches_for_field_value(&single)? {
                found_any = true;
                for (seg_id, bitmap) in matches {
                    let entry = combined.entry(seg_id).or_insert_with(RoaringBitmap::new);
                    *entry |= bitmap;
                }
            }
        }

        if found_any {
            let result: Vec<(u64, RoaringBitmap)> = combined.into_iter().filter(|(_, bm)| !bm.is_empty()).collect();
            if result.is_empty() {
                Ok(Some(Vec::new()))
            } else {
                Ok(Some(result))
            }
        } else {
            Ok(None)
        }
    }

    fn exact_matches_for_field_value(&self, filter: &Filter) -> Result<Option<Vec<(u64, RoaringBitmap)>>> {
        let immutable_index = self.load_exact_index(&filter.field)?;
        let mut matches = Vec::new();
        let mut found_exact_key = false;

        {
            let guard = immutable_index.read().unwrap();
            if let Some(entries) = guard.get(&filter.value) {
                found_exact_key = true;
                for (segment_id, bitmap) in entries.iter() {
                    let live_bitmap = self.live_bitmap_for_segment(*segment_id, bitmap);
                    if !live_bitmap.is_empty() {
                        matches.push((*segment_id, live_bitmap));
                    }
                }
            }
        }

        if let Some(writer) = &self.active_segment {
            if let Some(field_bitmaps) = writer.bitmaps.get(&filter.field) {
                if let Some(bitmap) = field_bitmaps.get(&filter.value) {
                    found_exact_key = true;
                    let mut live_bitmap = bitmap.clone();
                    if !writer.segment.deleted_bitmap.is_empty() {
                        live_bitmap -= &writer.segment.deleted_bitmap;
                    }
                    if !live_bitmap.is_empty() {
                        matches.push((writer.segment.id, live_bitmap));
                    }
                }
            }
        }

        if found_exact_key {
            Ok(Some(matches))
        } else {
            // Without confirmed bitmap hits, do not assume "zero rows": persisted indexes can omit active CDC rows.
            Ok(None)
        }
    }

    fn live_bitmap_for_segment(&self, segment_id: u64, bitmap: &RoaringBitmap) -> RoaringBitmap {
        if let Some(writer) = &self.active_segment {
            if writer.segment.id == segment_id {
                let mut live = bitmap.clone();
                if !writer.segment.deleted_bitmap.is_empty() {
                    live -= &writer.segment.deleted_bitmap;
                }
                return live;
            }
        }

        if let Some(segment) = self.immutable_segments.iter().find(|segment| segment.id == segment_id) {
            let mut live = bitmap.clone();
            if !segment.deleted_bitmap.is_empty() {
                live -= &segment.deleted_bitmap;
            }
            return live;
        }

        // Segment no longer exists (compact removed it but the on-disk exact_index still has
        // entries pointing to its old id). Returning the original bitmap inflates total_found
        // and produces phantom matches that resolve to empty rows during get_rows_batch, which
        // forces the join executor to page through non-existent data — turning sub-100ms
        // queries into multi-second ones. Drop the stale entry instead.
        RoaringBitmap::new()
    }

    fn scan_segments_parallel(
        &self,
        segment_tasks: &[&Segment],
        filters: &[Filter],
        filters_op: &LogicalOp,
    ) -> Result<Vec<(u64, RoaringBitmap)>> {
        let segment_results: Vec<Result<(u64, RoaringBitmap)>> = segment_tasks.par_iter()
            .map(|segment| {
                let bitmap = if let Some(writer) = &self.active_segment {
                    if writer.segment.id == segment.id { writer.search(filters, filters_op)? }
                    else { segment.search_thread_safe(filters, filters_op)? }
                } else { segment.search_thread_safe(filters, filters_op)? };
                Ok((segment.id, bitmap))
            })
            .collect();
        let mut matches = Vec::new();
        for res in segment_results { let (id, bitmap) = res?; if !bitmap.is_empty() { matches.push((id, bitmap)); } }
        Ok(matches)
    }

    fn process_seg_sum_grouped(&self, seg_id: u64, bitmap: &RoaringBitmap, field_name: &str, required_fields: &[String], expr: &crate::core::expression::Expr) -> HashMap<String, f64> {
        let mut seg_group_sums = HashMap::new();
        let segment = if let Some(writer) = &self.active_segment { if writer.segment.id == seg_id { Some(&writer.segment) } else { self.immutable_segments.iter().find(|s| s.id == seg_id) } }
        else { self.immutable_segments.iter().find(|s| s.id == seg_id) };
        if let Some(s) = segment {
            // Pre-load all necessary mmaps for grouping and expressions
            let group_mmap = s.get_mmap_pair(field_name).ok();
            let mmaps: Vec<Option<Arc<(Mmap, Mmap)>>> = required_fields.iter().map(|f| s.get_mmap_pair(f).ok()).collect();
            
            // Check if it's a simple Field(name) expression to avoid the generic evaluate() overhead
            let simple_field = match expr {
                crate::core::expression::Expr::Field(f) => Some(f.clone()),
                _ => None,
            };

            let mut context = HashMap::with_capacity(required_fields.len());
            for id in bitmap {
                let group_val = if let Some(m) = &group_mmap { 
                    s.get_row_values_from_mmaps(id, &[Some(m.clone())]).pop().unwrap_or_else(|| "Unknown".to_string()) 
                } else { "Unknown".to_string() };

                let val = if let Some(f_name) = &simple_field {
                    // Direct numeric access if possible
                    if let Some(idx) = required_fields.iter().position(|r| r == f_name) {
                        s.get_row_numbers_from_mmaps(id, &[mmaps[idx].clone()]).pop().unwrap_or(0.0)
                    } else { 0.0 }
                } else {
                    let row_nums = s.get_row_numbers_from_mmaps(id, &mmaps);
                    context.clear();
                    for (i, v) in row_nums.into_iter().enumerate() { context.insert(required_fields[i].clone(), v); }
                    crate::core::expression::evaluate(expr, &context)
                };
                
                *seg_group_sums.entry(group_val).or_insert(0.0) += val;
            }
        }
        seg_group_sums
    }

    fn process_seg_sum_global(&self, seg_id: u64, bitmap: &RoaringBitmap, required_fields: &[String], expr: &crate::core::expression::Expr) -> f64 {
        let segment = if let Some(writer) = &self.active_segment { if writer.segment.id == seg_id { Some(&writer.segment) } else { self.immutable_segments.iter().find(|s| s.id == seg_id) } }
        else { self.immutable_segments.iter().find(|s| s.id == seg_id) };
        let mut seg_sum = 0.0;
        if let Some(s) = segment {
            let mmaps: Vec<Option<Arc<(Mmap, Mmap)>>> = required_fields.iter().map(|f| s.get_mmap_pair(f).ok()).collect();
            let mut context = HashMap::with_capacity(required_fields.len());
            for id in bitmap {
                let row_nums = s.get_row_numbers_from_mmaps(id, &mmaps);
                context.clear();
                for (i, val) in row_nums.into_iter().enumerate() { context.insert(required_fields[i].clone(), val); }
                seg_sum += crate::core::expression::evaluate(expr, &context);
            }
        }
        seg_sum
    }

    pub fn get_indexed_fields_static(entity: &str, table: &str) -> Vec<String> {
        let table_path = crate::core::data_paths::mirror_entity_dir(entity).join(table);
        let mut all_fields = std::collections::HashSet::new();
        
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

    pub fn get_base_fields_static(all_fields: &[String]) -> Vec<String> {
        let mut filtered: Vec<String> = all_fields.iter()
            .filter(|f| {
                !f.ends_with("_day") && !f.ends_with("_month") && 
                !f.ends_with("_year") && !f.ends_with("_hour_bucket")
            })
            .cloned().collect();
        filtered.sort();
        filtered
    }

    pub fn get_all_fields(&self) -> Vec<String> {
        let entity = self.base_path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        Self::get_indexed_fields_static(entity, &self.name)
    }

    /// Compact all immutable segments into larger segments.
    ///
    /// Merges the existing immutable segments into fewer, larger segments whose
    /// target row-count is controlled by the `BITTICE_COMPACT_SEGMENT_ROWS`
    /// environment variable (default 50,000).
    ///
    /// Returns the number of segments removed (old_count - new_count).
    ///
    /// Safety: must be called when no concurrent writes are happening.
    pub fn compact(&mut self) -> Result<usize> {
        // 1. Flush active segment so every row is in an immutable segment.
        self.flush_active_segment()?;

        let old_count = self.immutable_segments.len();

        // 2. Skip-early check.
        let target_rows: u64 = std::env::var("BITTICE_COMPACT_SEGMENT_ROWS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(50_000);

        if old_count < 4 {
            info!("Table '{}': compact() skipped — only {} immutable segment(s).", self.name, old_count);
            return Ok(0);
        }

        let total_rows: u64 = self.immutable_segments.iter()
            .map(|s| s.record_count.saturating_sub(s.deleted_bitmap.len()))
            .sum();

        let ideal_segments = total_rows.div_ceil(target_rows).max(1);
        // Skip if already compact enough (fits in 2x the ideal count).
        if old_count as u64 <= ideal_segments * 2 {
            info!(
                "Table '{}': compact() skipped — {} segments already close to ideal {} for {} rows.",
                self.name, old_count, ideal_segments, total_rows
            );
            return Ok(0);
        }

        info!(
            "Table '{}': compacting {} segments ({} live rows) into ~{} segment(s) of {} rows each.",
            self.name, old_count, total_rows, ideal_segments, target_rows
        );

        // 3. Collect field names from the first segment's directory listing.
        let fields: Vec<String> = if let Some(first_seg) = self.immutable_segments.first() {
            let mut found: Vec<String> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&first_seg.path) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.ends_with(".dat") && !name.starts_with("bitmaps_") {
                            found.push(name.trim_end_matches(".dat").to_string());
                        }
                    }
                }
            }
            found.sort();
            found
        } else {
            return Ok(0);
        };

        if fields.is_empty() {
            return Ok(0);
        }

        // 4. Write compacted segments into a temporary directory, then rename into place.
        let segments_dir = self.base_path.join("segments");
        let tmp_dir = self.base_path.join("segments_compact_tmp");
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir)?;
        }
        fs::create_dir_all(&tmp_dir)?;

        // Choose starting ID well past all existing segment IDs to avoid collisions.
        let max_existing_id = self.immutable_segments.iter().map(|s| s.id).max().unwrap_or(0);
        let mut next_new_id = max_existing_id + 1_000_000;

        let pk_field = if self.manifest.primary_key.is_empty() {
            "PK".to_string()
        } else {
            self.manifest.primary_key.clone()
        };

        // pk_val -> (new_seg_id, new_local_id)
        let mut new_primary_index = PrimaryIndex::new();

        // Collect new segments as (SegmentWriter, bitmaps) so we can finalize them.
        let mut finished_writers: Vec<SegmentWriter> = Vec::new();

        // Current writer being filled.
        let mut current_writer: Option<SegmentWriter> = None;
        let mut current_row_count: u64 = 0;

        // Build a set of alive (seg_id, local_id) from primary_index.
        // This is necessary because deleted_bitmap for immutable segments
        // may not be persisted to disk when deletions happen in a later session
        // (the bitmap is only in-memory). The primary_index always reflects
        // the true set of live rows.
        let alive_positions: std::collections::HashSet<(u64, u32)> = self.primary_index.inner().values().copied().collect();

        // Iterate over all immutable segments in order.
        for seg in &self.immutable_segments {
            // Ensure mmap files are mapped.
            let _ = seg.ensure_mmaps_batch(&fields);

            for local_id in 0u32..seg.record_count as u32 {
                // Skip deleted rows: check both the segment's in-memory bitmap
                // and the primary_index-derived alive set (for robustness after reopen).
                if seg.deleted_bitmap.contains(local_id) || !alive_positions.contains(&(seg.id, local_id)) {
                    continue;
                }

                // Start a new compact segment when needed.
                if current_writer.is_none() || current_row_count >= target_rows {
                    // Finish the current writer.
                    if let Some(writer) = current_writer.take() {
                        finished_writers.push(writer);
                    }

                    let new_seg = Segment::new(next_new_id, &tmp_dir);
                    new_seg.create_dirs()?;
                    current_writer = Some(SegmentWriter::new(new_seg));
                    current_row_count = 0;
                    next_new_id += 1;
                }

                // Read all field values for this row.
                let row_values = seg.get_row_values(local_id, &fields)?;
                let mut row_map: HashMap<String, String> = HashMap::with_capacity(fields.len());
                for (i, field) in fields.iter().enumerate() {
                    row_map.insert(field.clone(), row_values.get(i).cloned().unwrap_or_default());
                }

                let writer = current_writer.as_mut().unwrap();
                let new_local_id = writer.segment.record_count as u32;
                let new_seg_id = writer.segment.id;

                writer.append_record(&row_map)?;
                current_row_count += 1;

                // Get PK value directly from the row data.
                // pk_field is always in fields (derived from segment directory),
                // so row_map always contains it.
                if let Some(pk_val) = row_map.get(&pk_field) {
                    new_primary_index.insert(pk_val, (new_seg_id, new_local_id));
                }
            }
        }

        // Finish the last writer.
        if let Some(writer) = current_writer.take() {
            finished_writers.push(writer);
        }

        if finished_writers.is_empty() {
            // All rows were deleted — still clean up old segments.
            info!("Table '{}': compact() found no live rows; removing all old segments.", self.name);
        }

        // 5. Flush all new writers and collect bitmaps.
        for writer in &mut finished_writers {
            writer.flush()?;
            // Persist final bitmaps unconditionally (flush() only writes periodically).
            for (col, map) in &writer.bitmaps {
                let path = writer.segment.path.join(format!("bitmaps_{}.dat", col));
                let file = fs::File::create(path)?;
                bincode::serialize_into(file, map)?;
            }
        }

        // 6. Move new segments from tmp dir into the real segments dir.
        //    Use final IDs starting right after the old highest segment ID so they
        //    sort after everything that exists, avoiding any ID collision.
        let old_ids: Vec<u64> = self.immutable_segments.iter().map(|s| s.id).collect();
        let base_final_id = self.manifest.active_segment_id + 1;
        // We'll remap the temporary IDs to final sequential IDs.
        // Collect temp-id -> final-id mapping.
        let mut temp_to_final: HashMap<u64, u64> = HashMap::new();
        let mut final_id_cursor = base_final_id;
        for writer in &finished_writers {
            temp_to_final.insert(writer.segment.id, final_id_cursor);
            final_id_cursor += 1;
        }

        // Rename temp segment dirs to final names.
        for writer in &finished_writers {
            let temp_seg_path = &writer.segment.path; // tmp_dir/seg_XXXXXX
            let final_id = temp_to_final[&writer.segment.id];
            let final_seg_path = segments_dir.join(format!("seg_{:04}", final_id));
            // A previous interrupted compact may have left data at this path; clear it first
            // so that rename() doesn't fail with ENOTEMPTY on macOS / Linux.
            if final_seg_path.exists() {
                fs::remove_dir_all(&final_seg_path)?;
            }
            fs::rename(temp_seg_path, &final_seg_path)?;
        }
        // Remove the tmp directory (should be empty now).
        let _ = fs::remove_dir(&tmp_dir);

        // 7. Update primary_index IDs from temp to final.
        for (_, (seg_id, _)) in new_primary_index.iter_mut() {
            if let Some(&final_id) = temp_to_final.get(seg_id) {
                *seg_id = final_id;
            }
        }

        // 8. Load new segments as immutable Segment structs.
        let mut new_immutable_segments: Vec<Segment> = Vec::with_capacity(finished_writers.len());
        for writer in &finished_writers {
            let final_id = temp_to_final[&writer.segment.id];
            let final_seg_path = segments_dir.join(format!("seg_{:04}", final_id));
            // Build a SegmentMeta from the writer's in-memory state.
            let meta = crate::core::storage::manifest::SegmentMeta {
                id: final_id,
                min_max: writer.segment.min_max.clone(),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                record_count: writer.segment.record_count,
                deleted_count: 0,
                path: format!("seg_{:04}", final_id),
            };
            let mut seg = Segment::load(&final_seg_path, Some(&meta))?;
            seg.is_immutable = true;
            new_immutable_segments.push(seg);
        }

        // 9. Build exact indexes for each new segment.
        for (idx, writer) in finished_writers.iter().enumerate() {
            let final_id = temp_to_final[&writer.segment.id];
            // Build a bitmaps map with final IDs for merge_exact_indexes_for_segment.
            self.merge_exact_indexes_for_segment(final_id, &writer.bitmaps)?;
            // Also update the segment's ID reference in our new list.
            new_immutable_segments[idx].id = final_id;
        }

        // 10. Delete old segment directories from disk.
        for old_id in &old_ids {
            let old_seg_path = segments_dir.join(format!("seg_{:04}", old_id));
            if old_seg_path.exists() {
                fs::remove_dir_all(&old_seg_path)?;
            }
        }

        // 11. Update manifest: remove old segments, add new ones, bump active_segment_id.
        for old_id in &old_ids {
            self.manifest.remove_segment(*old_id);
        }
        for seg in &new_immutable_segments {
            let meta = seg.to_meta();
            self.manifest.add_segment(meta);
        }
        // Advance active_segment_id past all new compact segments.
        self.manifest.active_segment_id = final_id_cursor;
        self.manifest.last_sequence_number += 1;

        // 12. Replace in-memory immutable segments.
        // Drop old mmaps first to free file handles.
        for seg in &mut self.immutable_segments {
            seg.mmap_cache.write().unwrap().clear();
            seg.mmap_access_order.write().unwrap().clear();
            seg.bitmap_cache.write().unwrap().clear();
        }
        self.immutable_segments = new_immutable_segments;

        // 13. Replace primary index, persist cache (with new compact segments), purge stale
        //     entries, and clear cache.
        self.primary_index = new_primary_index;
        // Flush cache to disk first so the on-disk index reflects the new compact segments
        // (merge_exact_indexes_for_segment skipped disk writes on the CDC hot path).
        self.persist_all_exact_indexes()?;
        self.purge_exact_indexes_for_removed_segments(&old_ids)?;
        self.exact_index_cache.write().unwrap().clear();

        // 14. Save manifest and primary index; truncate WAL.
        self.save_manifest()?;
        self.save_primary_index()?;
        self.wal.flush_writes()?;
        self.wal.truncate()?;

        // 15. Re-create active segment for subsequent writes.
        self.active_segment = None;
        self.ensure_active_segment()?;

        let new_count = self.immutable_segments.len();
        let reduced = old_count.saturating_sub(new_count);

        info!(
            "Table '{}': compaction complete. {} -> {} segments ({} reduced).",
            self.name, old_count, new_count, reduced
        );

        Ok(reduced)
    }
}

fn intersect_segment_matches(
    left: Vec<(u64, RoaringBitmap)>,
    right: Vec<(u64, RoaringBitmap)>,
) -> Vec<(u64, RoaringBitmap)> {
    let right_map: HashMap<u64, RoaringBitmap> = right.into_iter().collect();
    let mut matches = Vec::new();

    for (segment_id, left_bitmap) in left {
        if let Some(right_bitmap) = right_map.get(&segment_id) {
            let intersection = &left_bitmap & right_bitmap;
            if !intersection.is_empty() {
                matches.push((segment_id, intersection));
            }
        }
    }

    matches
}
