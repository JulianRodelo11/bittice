use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use lru::LruCache;
use parking_lot::Mutex;
use roaring::RoaringBitmap;
use tracing::warn;
use xxhash_rust::xxh3::xxh3_128;

use crate::core::storage::canonical::canonical_bytes;
use crate::core::storage::exact_index_v3::{
    write_exact_index_v3, write_exact_index_v3_from_hashmap, SnapshotReader,
};

const DEFAULT_CACHE_SIZE: usize = 1000;

type BitmapList = Arc<Vec<(u64, RoaringBitmap)>>;
type EntryCache = LruCache<u128, BitmapList>;
type SnapEntryIter =
    std::vec::IntoIter<std::result::Result<(u128, Vec<(u64, RoaringBitmap)>), crate::core::storage::exact_index_v3::format::FormatError>>;

const EXACT_IDX_MAGIC: [u8; 4] = *b"BTXI";
const EXACT_IDX_VERSION_V1: u8 = 1;
const EXACT_IDX_VERSION_V2: u8 = 2;
const EXACT_IDX_VERSION_V3: u8 = 3;

enum DeltaEntry {
    Replace(BitmapList),
    Removed,
}

impl Clone for DeltaEntry {
    fn clone(&self) -> Self {
        match self {
            DeltaEntry::Replace(arc) => DeltaEntry::Replace(Arc::clone(arc)),
            DeltaEntry::Removed => DeltaEntry::Removed,
        }
    }
}

/// Exact-match index for a single field.
///
/// Maps each distinct field value (by xxh3_128(NFC(value))) to a list of
/// `(segment_id, bitmap)` pairs.  Persisted data uses the v3 mmap snapshot
/// format; in-memory changes accumulate in `delta` until the next [`save`].
pub struct ExactIndex {
    snapshot: Option<SnapshotReader>,
    path: Option<PathBuf>,
    delta: HashMap<u128, DeltaEntry>,
    cache: Mutex<EntryCache>,
}

impl std::fmt::Debug for ExactIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExactIndex")
            .field("has_snapshot", &self.snapshot.is_some())
            .field("path", &self.path)
            .field("delta_len", &self.delta.len())
            .field("cache_len", &self.cache.lock().len())
            .finish()
    }
}

impl Default for ExactIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ExactIndex {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            path: None,
            delta: HashMap::new(),
            cache: Mutex::new(LruCache::new(cache_capacity())),
        }
    }

    /// Open an exact index from disk, or prepare an empty index bound to `path`
    /// when the file does not exist yet (first save will create it).
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                snapshot: None,
                path: Some(path.to_path_buf()),
                delta: HashMap::new(),
                cache: Mutex::new(LruCache::new(cache_capacity())),
            });
        }

        let mut file = BufReader::new(
            fs::File::open(path).with_context(|| format!("open exact index {:?}", path))?,
        );
        let mut header = [0u8; 8];
        let n = file.read(&mut header)?;

        let mut idx = Self {
            snapshot: None,
            path: Some(path.to_path_buf()),
            delta: HashMap::new(),
            cache: Mutex::new(LruCache::new(cache_capacity())),
        };

        if n >= 4 && header[..4] == EXACT_IDX_MAGIC {
            let version = header[4];
            match version {
                EXACT_IDX_VERSION_V3 => {
                    idx.snapshot = Some(
                        SnapshotReader::open(path)
                            .map_err(|e| anyhow::anyhow!("open v3 snapshot {:?}: {}", path, e))?,
                    );
                }
                EXACT_IDX_VERSION_V2 => {
                    let map: HashMap<u128, Vec<(u64, RoaringBitmap)>> =
                        bincode::deserialize_from(file)
                            .context("deserialize exact index v2 payload")?;
                    for (hash, bitmaps) in map {
                        idx.delta
                            .insert(hash, DeltaEntry::Replace(Arc::new(bitmaps)));
                    }
                }
                EXACT_IDX_VERSION_V1 => {
                    warn!(
                        "exact index at {:?} is v1 (String keys); migrating to v3 on next save",
                        path
                    );
                    let raw: HashMap<String, Vec<(u64, RoaringBitmap)>> =
                        bincode::deserialize_from(file)
                            .context("deserialize exact index v1 payload")?;
                    load_v1_into_delta(&mut idx.delta, raw, path);
                }
                v => {
                    return Err(anyhow::anyhow!(
                        "exact index version {} not supported (path: {:?})",
                        v,
                        path
                    ));
                }
            }
        } else {
            warn!(
                "exact index at {:?} is legacy (no header); migrating to v3 on next save",
                path
            );
            let file = BufReader::new(
                fs::File::open(path).with_context(|| format!("reopen legacy {:?}", path))?,
            );
            let raw: HashMap<String, Vec<(u64, RoaringBitmap)>> =
                bincode::deserialize_from(file).context("deserialize legacy exact index")?;
            load_v1_into_delta(&mut idx.delta, raw, path);
        }

        Ok(idx)
    }

    pub fn hash(value: &str) -> u128 {
        xxh3_128(&canonical_bytes(value))
    }

    pub fn get(&self, value: &str) -> Option<Arc<Vec<(u64, RoaringBitmap)>>> {
        let hash = Self::hash(value);

        match self.delta.get(&hash) {
            Some(DeltaEntry::Replace(arc)) => return Some(Arc::clone(arc)),
            Some(DeltaEntry::Removed) => return None,
            None => {}
        }

        if let Some(arc) = self.cache.lock().get(&hash) {
            return Some(Arc::clone(arc));
        }

        let snap = self.snapshot.as_ref()?;
        match snap.get(hash) {
            Ok(Some(bitmaps)) => {
                let arc = Arc::new(bitmaps);
                self.cache.lock().put(hash, Arc::clone(&arc));
                Some(arc)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("exact index snapshot get failed for hash {hash:#x}: {e}");
                None
            }
        }
    }

    pub fn insert(&mut self, value: &str, bitmaps: Vec<(u64, RoaringBitmap)>) {
        let hash = Self::hash(value);
        self.delta
            .insert(hash, DeltaEntry::Replace(Arc::new(bitmaps)));
        self.cache.lock().pop(&hash);
    }

    pub fn merge_segment(&mut self, seg_id: u64, segment_bitmaps: HashMap<String, RoaringBitmap>) {
        for (value, bitmap) in segment_bitmaps {
            let hash = Self::hash(&value);

            let mut current_bitmaps = match self.delta.get(&hash) {
                Some(DeltaEntry::Replace(arc)) => (**arc).clone(),
                Some(DeltaEntry::Removed) => Vec::new(),
                None => self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.get(hash).ok().flatten())
                    .unwrap_or_default(),
            };

            if let Some(existing) = current_bitmaps.iter_mut().find(|(s, _)| *s == seg_id) {
                existing.1 |= bitmap;
            } else {
                current_bitmaps.push((seg_id, bitmap));
            }

            self.delta
                .insert(hash, DeltaEntry::Replace(Arc::new(current_bitmaps)));
            self.cache.lock().pop(&hash);
        }
    }

    /// Remove all entries that reference any of the given segment IDs.
    ///
    /// Scans the full snapshot and delta (O(N)). Returns `true` if any entry changed.
    pub fn purge_segments(&mut self, removed_seg_ids: &HashSet<u64>) -> bool {
        let mut changed = false;

        if let Some(snapshot) = &self.snapshot {
            let mut to_apply: Vec<(u128, DeltaEntry)> = Vec::new();
            for result in snapshot.iter_entries() {
                let (hash, bitmaps) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!("exact index purge: snapshot iter error: {e}");
                        continue;
                    }
                };
                if self.delta.contains_key(&hash) {
                    continue;
                }
                if !bitmaps.iter().any(|(seg_id, _)| removed_seg_ids.contains(seg_id)) {
                    continue;
                }
                let filtered: Vec<(u64, RoaringBitmap)> = bitmaps
                    .into_iter()
                    .filter(|(seg_id, _)| !removed_seg_ids.contains(seg_id))
                    .collect();
                let entry = if filtered.is_empty() {
                    DeltaEntry::Removed
                } else {
                    DeltaEntry::Replace(Arc::new(filtered))
                };
                to_apply.push((hash, entry));
            }
            if !to_apply.is_empty() {
                changed = true;
                for (hash, entry) in to_apply {
                    self.delta.insert(hash, entry);
                }
            }
        }

        let keys: Vec<u128> = self.delta.keys().copied().collect();
        for hash in keys {
            let new_entry = match self.delta.get(&hash) {
                Some(DeltaEntry::Replace(arc)) => {
                    let filtered: Vec<(u64, RoaringBitmap)> = arc
                        .iter()
                        .filter(|(seg_id, _)| !removed_seg_ids.contains(seg_id))
                        .map(|(s, b)| (*s, b.clone()))
                        .collect();
                    if filtered.len() == arc.len() {
                        continue;
                    }
                    changed = true;
                    if filtered.is_empty() {
                        Some(DeltaEntry::Removed)
                    } else {
                        Some(DeltaEntry::Replace(Arc::new(filtered)))
                    }
                }
                Some(DeltaEntry::Removed) => continue,
                None => continue,
            };
            if let Some(entry) = new_entry {
                self.delta.insert(hash, entry);
            }
        }

        if changed {
            self.cache.lock().clear();
        }
        changed
    }

    pub fn save(&mut self, path: Option<&Path>) -> Result<()> {
        let target_path = match path {
            Some(p) => p,
            None => self
                .path
                .as_deref()
                .context("exact index save: no path set")?,
        };

        if self.delta.is_empty() && self.snapshot.is_some() {
            return Ok(());
        }

        if let Some(parent) = target_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create dir {:?}", parent))?;
            }
        }

        if let Some(snapshot) = &self.snapshot {
            let merged_iter = merge_streams(snapshot, &self.delta)?;
            write_exact_index_v3(target_path, merged_iter)
                .map_err(|e| anyhow::anyhow!("write exact index v3 {:?}: {}", target_path, e))?;
        } else {
            let map: HashMap<u128, Vec<(u64, RoaringBitmap)>> = self
                .delta
                .drain()
                .filter_map(|(k, v)| match v {
                    DeltaEntry::Replace(arc) => Some((
                        k,
                        Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone()),
                    )),
                    DeltaEntry::Removed => None,
                })
                .collect();
            write_exact_index_v3_from_hashmap(target_path, map).map_err(|e| {
                anyhow::anyhow!("write exact index v3 {:?}: {}", target_path, e)
            })?;
        }

        self.snapshot = None;
        self.delta.clear();
        self.cache.lock().clear();
        self.snapshot = Some(
            SnapshotReader::open(target_path)
                .map_err(|e| anyhow::anyhow!("reopen v3 snapshot {:?}: {}", target_path, e))?,
        );
        self.path = Some(target_path.to_path_buf());
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.delta.values().all(|e| matches!(e, DeltaEntry::Removed))
            && self
                .snapshot
                .as_ref()
                .is_none_or(|s| s.num_entries() == 0)
    }

    /// Returns an approximate count of distinct values. Exact only when delta is empty.
    /// When delta has changes, may over-count (new hashes are added without checking
    /// snapshot overlap) or under-count (removed entries are subtracted). For exact
    /// count, save() and reopen.
    pub fn len(&self) -> usize {
        let snap = self.snapshot.as_ref().map_or(0, |s| s.num_entries() as usize);
        let delta_added = self
            .delta
            .values()
            .filter(|e| matches!(e, DeltaEntry::Replace(_)))
            .count();
        let delta_removed = self
            .delta
            .values()
            .filter(|e| matches!(e, DeltaEntry::Removed))
            .count();
        snap.saturating_add(delta_added).saturating_sub(delta_removed)
    }

    /// Number of entries currently in the LRU cache (for tests and diagnostics).
    pub fn cache_len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Number of pending in-memory changes not yet persisted.
    pub fn delta_len(&self) -> usize {
        self.delta.len()
    }

    /// Union of segment IDs referenced by any entry in this index (snapshot + delta).
    ///
    /// Used by `Table::load_exact_index` to detect when the on-disk index is missing
    /// segments that the table's manifest knows about — which happens after any
    /// non-clean shutdown, since `merge_exact_indexes_for_segment` only updates the
    /// in-memory cache and disk persistence is deferred to `compact()`/`close()`.
    /// Without this check, post-restart queries hit only the segments that were
    /// already persisted, miss live rows in newer segments, and return empty.
    pub fn segment_ids(&self) -> HashSet<u64> {
        let mut ids: HashSet<u64> = HashSet::new();

        if let Some(snapshot) = &self.snapshot {
            for result in snapshot.iter_entries() {
                let (hash, bitmaps) = match result {
                    Ok(pair) => pair,
                    Err(_) => continue,
                };
                // delta overrides snapshot for the same hash; honor that.
                if self.delta.contains_key(&hash) {
                    continue;
                }
                for (seg_id, _) in bitmaps {
                    ids.insert(seg_id);
                }
            }
        }

        for entry in self.delta.values() {
            if let DeltaEntry::Replace(arc) = entry {
                for (seg_id, _) in arc.iter() {
                    ids.insert(*seg_id);
                }
            }
        }

        ids
    }

    pub(crate) fn bind_path(&mut self, path: PathBuf) {
        self.path = Some(path);
    }
}

fn cache_capacity() -> NonZeroUsize {
    std::env::var("BITTICE_EXACT_INDEX_CACHE_PER_FIELD")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .and_then(NonZeroUsize::new)
        .unwrap_or_else(|| NonZeroUsize::new(DEFAULT_CACHE_SIZE).unwrap())
}

fn load_v1_into_delta(
    delta: &mut HashMap<u128, DeltaEntry>,
    raw: HashMap<String, Vec<(u64, RoaringBitmap)>>,
    path: &Path,
) {
    let mut collapsed = 0usize;
    for (value, bitmaps) in raw {
        let hash = ExactIndex::hash(&value);
        if let Some(DeltaEntry::Replace(arc)) = delta.get_mut(&hash) {
            collapsed += 1;
            let entry = Arc::make_mut(arc);
            for (seg_id, bitmap) in bitmaps {
                if let Some(existing) = entry.iter_mut().find(|(s, _)| *s == seg_id) {
                    existing.1 |= bitmap;
                } else {
                    entry.push((seg_id, bitmap));
                }
            }
        } else {
            delta.insert(hash, DeltaEntry::Replace(Arc::new(bitmaps)));
        }
    }
    if collapsed > 0 {
        warn!(
            "exact index migration {:?}: {} entries collapsed (NFC normalization merged distinct \
             String keys to the same hash). This is expected when migrating NFD/NFC variants.",
            path, collapsed
        );
    }
}

struct MergedEntriesIter {
    snap: SnapEntryIter,
    delta: Vec<(u128, DeltaEntry)>,
    delta_idx: usize,
    snap_buf: Option<(u128, Vec<(u64, RoaringBitmap)>)>,
}

impl Iterator for MergedEntriesIter {
    type Item = (u128, Vec<(u64, RoaringBitmap)>);

    fn next(&mut self) -> Option<Self::Item> {
        let snap_hash = self.snap_buf.as_ref().map(|(h, _)| *h);
        let delta_hash = self.delta.get(self.delta_idx).map(|(h, _)| *h);

        match (snap_hash, delta_hash) {
            (None, None) => None,
            (Some(sh), None) => {
                let (_, bitmaps) = self.snap_buf.take().unwrap();
                self.advance_snap();
                Some((sh, bitmaps))
            }
            (None, Some(dh)) => self.take_delta(dh),
            (Some(sh), Some(dh)) if sh < dh => {
                let (_, bitmaps) = self.snap_buf.take().unwrap();
                self.advance_snap();
                Some((sh, bitmaps))
            }
            (Some(sh), Some(dh)) if sh > dh => self.take_delta(dh),
            (Some(sh), Some(dh)) => {
                debug_assert_eq!(sh, dh);
                let _ = self.snap_buf.take();
                self.advance_snap();
                self.take_delta(dh)
            }
        }
    }
}

impl MergedEntriesIter {
    fn advance_snap(&mut self) {
        loop {
            match self.snap.next() {
                Some(Ok((hash, bitmaps))) => {
                    self.snap_buf = Some((hash, bitmaps));
                    break;
                }
                Some(Err(e)) => {
                    warn!("merge_streams: snapshot entry error: {e}");
                }
                None => {
                    self.snap_buf = None;
                    break;
                }
            }
        }
    }

    fn take_delta(&mut self, dh: u128) -> Option<(u128, Vec<(u64, RoaringBitmap)>)> {
        let (_, entry) = self.delta.get(self.delta_idx).unwrap();
        self.delta_idx += 1;
        match entry {
            DeltaEntry::Replace(arc) => Some((dh, (**arc).clone())),
            DeltaEntry::Removed => {
                self.next()
            }
        }
    }
}

fn merge_streams(
    snapshot: &SnapshotReader,
    delta: &HashMap<u128, DeltaEntry>,
) -> Result<MergedEntriesIter> {
    let mut delta_sorted: Vec<(u128, DeltaEntry)> =
        delta.iter().map(|(k, v)| (*k, v.clone())).collect();
    delta_sorted.sort_unstable_by_key(|(h, _)| *h);

    let snap_vec: Vec<_> = snapshot.iter_entries().collect();
    for result in &snap_vec {
        if let Err(e) = result {
            return Err(anyhow::anyhow!("snapshot iter: {}", e));
        }
    }

    let mut iter = MergedEntriesIter {
        snap: snap_vec.into_iter(),
        delta: delta_sorted,
        delta_idx: 0,
        snap_buf: None,
    };
    iter.advance_snap();
    Ok(iter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_index_basic_api() {
        let mut idx = ExactIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        assert!(idx.get("foo").is_none());

        let bm = {
            let mut b = RoaringBitmap::new();
            b.insert(0);
            b.insert(1);
            b
        };
        idx.insert("foo", vec![(1u64, bm.clone())]);
        assert!(!idx.is_empty());
        assert_eq!(idx.len(), 1);

        let entries = idx.get("foo").expect("should exist");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 1u64);
        assert_eq!(entries[0].1, bm);

        idx.insert("bar", vec![(2u64, RoaringBitmap::new())]);
        assert_eq!(idx.len(), 2);

        assert!(idx.get("missing").is_none());
    }

    #[test]
    fn exact_index_merge_via_merge_segment() {
        let mut idx = ExactIndex::new();

        let mut bm1 = RoaringBitmap::new();
        bm1.insert(10);
        let mut seg1_bitmaps = HashMap::new();
        seg1_bitmaps.insert("status".to_string(), bm1.clone());
        idx.merge_segment(1u64, seg1_bitmaps);

        let mut bm2 = RoaringBitmap::new();
        bm2.insert(20);
        bm2.insert(21);
        let mut seg2_bitmaps = HashMap::new();
        seg2_bitmaps.insert("status".to_string(), bm2.clone());
        idx.merge_segment(2u64, seg2_bitmaps);

        let entries = idx.get("status").expect("should exist");
        assert_eq!(entries.len(), 2, "two segments should be present");
        assert_eq!(entries[0].0, 1u64);
        assert_eq!(entries[0].1, bm1);
        assert_eq!(entries[1].0, 2u64);
        assert_eq!(entries[1].1, bm2);
    }

    #[test]
    fn exact_index_purge_via_purge_segments() {
        let mut idx = ExactIndex::new();

        let seg_ids_to_remove: HashSet<u64> = vec![2u64, 4u64].into_iter().collect();

        for val in &["active", "inactive", "pending"] {
            let mut seg_bitmaps = HashMap::new();
            for seg_id in 1u64..=5u64 {
                let mut bm = RoaringBitmap::new();
                bm.insert(seg_id as u32);
                seg_bitmaps.insert(val.to_string(), bm);
                idx.merge_segment(seg_id, seg_bitmaps.clone());
                seg_bitmaps.clear();
            }
        }

        assert_eq!(idx.get("active").unwrap().len(), 5);

        idx.purge_segments(&seg_ids_to_remove);

        for val in &["active", "inactive", "pending"] {
            let entries = idx.get(val).expect("value should still exist");
            assert_eq!(entries.len(), 3, "only 3 segments should remain for '{}'", val);
            let remaining_ids: Vec<u64> = entries.iter().map(|(id, _)| *id).collect();
            assert!(!remaining_ids.contains(&2), "seg 2 should be removed");
            assert!(!remaining_ids.contains(&4), "seg 4 should be removed");
        }
    }

    #[test]
    fn exact_index_nfc_nfd_same_hash() {
        let nfc = "caf\u{00e9}";
        let nfd = "cafe\u{0301}";
        assert_eq!(
            ExactIndex::hash(nfc),
            ExactIndex::hash(nfd),
            "NFC and NFD forms must produce the same hash"
        );

        let mut idx = ExactIndex::new();
        let mut bm = RoaringBitmap::new();
        bm.insert(1);
        idx.insert(nfd, vec![(1u64, bm)]);

        assert!(
            idx.get(nfc).is_some(),
            "NFC query must find the NFD-inserted entry"
        );
    }
}
