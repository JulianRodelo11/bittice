use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::canonical::canonical_bytes;
use xxhash_rust::xxh3::xxh3_128;

/// Encapsulates the table's primary key index.
///
/// Internally backed by `HashMap<u128, (segment_id, local_id)>` where the
/// key is `xxh3_128(canonical_bytes(pk))`. All public methods take `&str`
/// so that callers never interact with raw hashes.
///
/// CONTRACT: `hash()` is the SINGLE point where PKs are hashed. Any change
/// to `hash()` or `canonical_bytes()` requires a full migration of every
/// primary index on disk (v2 → v3).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrimaryIndex {
    inner: HashMap<u128, (u64, u32)>,
}

impl PrimaryIndex {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: HashMap::with_capacity(cap),
        }
    }

    /// Deterministic hash of a PK string. Private to this module.
    /// This is the SINGLE point where PKs are hashed.
    fn hash(pk: &str) -> u128 {
        xxh3_128(&canonical_bytes(pk))
    }

    pub fn insert(&mut self, pk: &str, loc: (u64, u32)) -> Option<(u64, u32)> {
        self.inner.insert(Self::hash(pk), loc)
    }

    pub fn get(&self, pk: &str) -> Option<(u64, u32)> {
        self.inner.get(&Self::hash(pk)).copied()
    }

    pub fn remove(&mut self, pk: &str) -> Option<(u64, u32)> {
        self.inner.remove(&Self::hash(pk))
    }

    pub fn contains_key(&self, pk: &str) -> bool {
        self.inner.contains_key(&Self::hash(pk))
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Mutable iterator over `(&u128, &mut (segment_id, local_id))`.
    ///
    /// Used in `compact()` to rewrite segment IDs after compaction.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u128, &mut (u64, u32))> + '_ {
        self.inner.iter_mut()
    }

    /// Insert with a raw hash key. Used during v1→v2 migration ONLY.
    ///
    /// Restricted to `pub(crate)` — do not expose outside the storage module.
    pub(crate) fn insert_raw(&mut self, hash: u128, loc: (u64, u32)) -> Option<(u64, u32)> {
        self.inner.insert(hash, loc)
    }

    /// Read-only access to the inner map. Used by IO serialization ONLY.
    pub(crate) fn inner(&self) -> &HashMap<u128, (u64, u32)> {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // API básica
    // -----------------------------------------------------------------------

    #[test]
    fn basic_api_roundtrip() {
        let mut idx = PrimaryIndex::new();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);

        assert_eq!(idx.insert("pk1", (1, 0)), None);
        assert_eq!(idx.insert("pk2", (1, 1)), None);
        assert_eq!(idx.insert("pk3", (2, 0)), None);
        assert_eq!(idx.len(), 3);
        assert!(!idx.is_empty());

        assert_eq!(idx.get("pk1"), Some((1, 0)));
        assert_eq!(idx.get("pk2"), Some((1, 1)));
        assert_eq!(idx.get("pk3"), Some((2, 0)));
        assert_eq!(idx.get("missing"), None);

        assert!(idx.contains_key("pk1"));
        assert!(!idx.contains_key("missing"));

        // Insert overwrite
        assert_eq!(idx.insert("pk1", (5, 99)), Some((1, 0)));
        assert_eq!(idx.get("pk1"), Some((5, 99)));
        assert_eq!(idx.len(), 3);

        // Remove
        assert_eq!(idx.remove("pk2"), Some((1, 1)));
        assert_eq!(idx.remove("pk2"), None);
        assert_eq!(idx.len(), 2);
        assert!(!idx.contains_key("pk2"));
    }

    // -----------------------------------------------------------------------
    // Serialize/deserialize roundtrip (v2 format: u128 keys)
    // -----------------------------------------------------------------------

    #[test]
    fn serialize_deserialize_roundtrip() {
        let mut idx = PrimaryIndex::new();
        idx.insert("alpha", (10, 0));
        idx.insert("beta", (20, 1));
        idx.insert("gamma", (30, 2));

        let bytes = bincode::serialize(&idx).expect("serialize");
        let decoded: PrimaryIndex = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded.get("alpha"), Some((10, 0)));
        assert_eq!(decoded.get("beta"), Some((20, 1)));
        assert_eq!(decoded.get("gamma"), Some((30, 2)));
    }

    // -----------------------------------------------------------------------
    // iter_mut (used by compact)
    // -----------------------------------------------------------------------

    #[test]
    fn iter_mut_allows_mutation() {
        let mut idx = PrimaryIndex::new();
        idx.insert("pk1", (1, 0));
        idx.insert("pk2", (2, 1));

        for (_, (seg_id, _)) in idx.iter_mut() {
            *seg_id += 100;
        }

        assert_eq!(idx.get("pk1"), Some((101, 0)));
        assert_eq!(idx.get("pk2"), Some((102, 1)));
    }

    // -----------------------------------------------------------------------
    // insert_raw (used by migration)
    // -----------------------------------------------------------------------

    #[test]
    fn insert_raw_works() {
        let mut idx = PrimaryIndex::new();
        let hash = xxh3_128(&canonical_bytes("test_pk"));
        idx.insert_raw(hash, (42, 7));

        assert_eq!(idx.get("test_pk"), Some((42, 7)));
    }

    // -----------------------------------------------------------------------
    // Hash determinism: same input → same hash
    // -----------------------------------------------------------------------

    #[test]
    fn hash_is_deterministic() {
        let pk = "my_test_pk_value";
        let h1 = PrimaryIndex::hash(pk);
        let h2 = PrimaryIndex::hash(pk);
        assert_eq!(h1, h2);
    }

    // -----------------------------------------------------------------------
    // NFC equivalence: NFC and NFD PKs hash to same bucket
    // -----------------------------------------------------------------------

    #[test]
    fn nfc_nfd_same_hash() {
        let nfc = "caf\u{00e9}";
        let nfd = "cafe\u{0301}";
        assert_eq!(
            PrimaryIndex::hash(nfc),
            PrimaryIndex::hash(nfd),
            "NFC and NFD forms must hash to the same value"
        );
    }

    // -----------------------------------------------------------------------
    // Distinct PKs produce distinct hashes (no collision for simple cases)
    // -----------------------------------------------------------------------

    #[test]
    fn distinct_pks_distinct_hashes() {
        let pks = ["pk1", "pk2", "pk3", "", "a", "b"];
        let mut hashes: Vec<u128> = pks.iter().map(|pk| PrimaryIndex::hash(pk)).collect();
        let len_before = hashes.len();
        hashes.sort();
        hashes.dedup();
        assert_eq!(hashes.len(), len_before, "all test PKs should have distinct hashes");
    }

    // -----------------------------------------------------------------------
    // with_capacity
    // -----------------------------------------------------------------------

    #[test]
    fn with_capacity_works() {
        let idx = PrimaryIndex::with_capacity(100);
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
    }
}
