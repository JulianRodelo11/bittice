use std::collections::HashMap;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use crate::core::storage::canonical::canonical_bytes;
use xxhash_rust::xxh3::xxh3_128;

/// Exact-match index for a single field.
///
/// Maps each distinct field value (by xxh3_128(NFC(value))) to a list of
/// `(segment_id, bitmap)` pairs, where the bitmap identifies the rows in that
/// segment that hold the value.
///
/// `#[serde(transparent)]` ensures that bincode serialization encodes the raw
/// `HashMap<u128, Vec<(u64, RoaringBitmap)>>` directly.  V2 format files use
/// this encoding.  V1/legacy files (String keys) are migrated by `exact_index_io`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExactIndex {
    inner: HashMap<u128, Vec<(u64, RoaringBitmap)>>,
}

impl ExactIndex {
    pub fn new() -> Self {
        Self { inner: HashMap::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self { inner: HashMap::with_capacity(cap) }
    }

    /// Hash a field value to its canonical u128 key.
    /// Uses xxh3_128(NFC(value)) so NFC and NFD forms produce the same key.
    pub(crate) fn hash(value: &str) -> u128 {
        xxh3_128(&canonical_bytes(value))
    }

    pub fn get(&self, value: &str) -> Option<&Vec<(u64, RoaringBitmap)>> {
        self.inner.get(&Self::hash(value))
    }

    pub fn insert(&mut self, value: &str, bitmaps: Vec<(u64, RoaringBitmap)>) {
        self.inner.insert(Self::hash(value), bitmaps);
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&u128, &mut Vec<(u64, RoaringBitmap)>) -> bool,
    {
        self.inner.retain(f);
    }

    /// Merge a segment's field→bitmap map into this index.
    ///
    /// For each (value, bitmap) pair: hashes `value`, then either appends a new
    /// `(seg_id, bitmap)` entry or ORs into an existing one if `seg_id` is
    /// already present for that hash.
    pub fn merge_segment(&mut self, seg_id: u64, segment_bitmaps: HashMap<String, RoaringBitmap>) {
        for (value, bitmap) in segment_bitmaps {
            let h = Self::hash(&value);
            let entry = self.inner.entry(h).or_insert_with(Vec::new);
            if let Some(existing) = entry.iter_mut().find(|(s, _)| *s == seg_id) {
                existing.1 |= bitmap;
            } else {
                entry.push((seg_id, bitmap));
            }
        }
    }

    /// Remove all entries that reference any of the given segment IDs.
    /// Entries whose segment list becomes empty are dropped entirely.
    pub fn purge_segments(&mut self, removed_seg_ids: &std::collections::HashSet<u64>) {
        self.inner.retain(|_hash, segments| {
            segments.retain(|(seg_id, _)| !removed_seg_ids.contains(seg_id));
            !segments.is_empty()
        });
    }

    pub fn is_empty(&self) -> bool { self.inner.is_empty() }
    pub fn len(&self) -> usize { self.inner.len() }

    /// Returns the inner HashMap. Used only during migration to v3 format.
    /// Transitional — will be removed in Phase 1d-integrate.
    pub(crate) fn into_inner(self) -> std::collections::HashMap<u128, Vec<(u64, RoaringBitmap)>> {
        self.inner
    }

    /// Used only by `exact_index_io` during v1→v2 migration.
    ///
    /// Inserts or merges a pre-hashed entry.  Returns `true` if there was
    /// already an entry for this hash (collision or NFC-collapse).
    pub(crate) fn insert_raw_or_merge(&mut self, hash: u128, bitmaps: Vec<(u64, RoaringBitmap)>) -> bool {
        let entry = self.inner.entry(hash).or_insert_with(Vec::new);
        let was_present = !entry.is_empty();
        for (seg_id, bitmap) in bitmaps {
            if let Some(existing) = entry.iter_mut().find(|(s, _)| *s == seg_id) {
                existing.1 |= bitmap;
            } else {
                entry.push((seg_id, bitmap));
            }
        }
        was_present
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test 1: basic API roundtrip
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Test 2: merge_segment — adds bitmaps across two segments for same value
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Test 3: retain — remove entries for specific segment IDs
    // -----------------------------------------------------------------------

    #[test]
    fn exact_index_purge_via_retain() {
        let mut idx = ExactIndex::new();

        let seg_ids_to_remove: std::collections::HashSet<u64> =
            vec![2u64, 4u64].into_iter().collect();

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

        // Each value should have 5 segment entries initially.
        assert_eq!(idx.get("active").unwrap().len(), 5);

        idx.retain(|_hash, segment_entries| {
            segment_entries.retain(|(seg_id, _)| !seg_ids_to_remove.contains(seg_id));
            !segment_entries.is_empty()
        });

        // After purge, each value keeps segments 1, 3, 5.
        for val in &["active", "inactive", "pending"] {
            let entries = idx.get(val).expect("value should still exist");
            assert_eq!(entries.len(), 3, "only 3 segments should remain for '{}'", val);
            let remaining_ids: Vec<u64> = entries.iter().map(|(id, _)| *id).collect();
            assert!(!remaining_ids.contains(&2), "seg 2 should be removed");
            assert!(!remaining_ids.contains(&4), "seg 4 should be removed");
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: serde roundtrip — serialize ExactIndex, deserialize ExactIndex, check equality
    // -----------------------------------------------------------------------

    #[test]
    fn exact_index_serde_roundtrip() {
        let mut idx = ExactIndex::new();

        let mut bm1 = RoaringBitmap::new();
        bm1.insert(100);
        bm1.insert(200);
        idx.insert("field_a", vec![(1u64, bm1.clone()), (2u64, RoaringBitmap::new())]);

        let mut bm2 = RoaringBitmap::new();
        bm2.insert(42);
        idx.insert("field_b", vec![(3u64, bm2.clone())]);

        let bytes = bincode::serialize(&idx).expect("serialize");
        let decoded: ExactIndex = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(decoded.len(), idx.len());

        let a_entries = decoded.get("field_a").expect("field_a should exist");
        assert_eq!(a_entries.len(), 2);
        assert_eq!(a_entries[0].0, 1u64);
        assert_eq!(a_entries[0].1, bm1);
        assert_eq!(a_entries[1].0, 2u64);
        assert!(a_entries[1].1.is_empty());

        let b_entries = decoded.get("field_b").expect("field_b should exist");
        assert_eq!(b_entries.len(), 1);
        assert_eq!(b_entries[0].0, 3u64);
        assert_eq!(b_entries[0].1, bm2);
    }

    // -----------------------------------------------------------------------
    // Test 5: NFC/NFD equivalence — same hash for canonically equivalent values
    // -----------------------------------------------------------------------

    #[test]
    fn exact_index_nfc_nfd_same_hash() {
        let nfc = "caf\u{00e9}";        // é precompuesto
        let nfd = "cafe\u{0301}";       // e + combining acute
        assert_eq!(
            ExactIndex::hash(nfc),
            ExactIndex::hash(nfd),
            "NFC and NFD forms must produce the same hash"
        );

        let mut idx = ExactIndex::new();
        let mut bm = RoaringBitmap::new();
        bm.insert(1);
        idx.insert(nfd, vec![(1u64, bm)]);

        // Query with NFC form — must find the entry inserted with NFD form.
        assert!(idx.get(nfc).is_some(), "NFC query must find NFD-inserted entry");
    }
}
