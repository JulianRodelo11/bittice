//! Phase 1d-format integration tests for the v3 exact index format.
//!
//! Tests the standalone v3 writer/reader WITHOUT touching the existing
//! ExactIndex struct or its callers.  Validates the v3 format end-to-end
//! before integration.

use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::io::{BufWriter, Write};
use xxhash_rust::xxh3::xxh3_128;

use bittice::core::storage::exact_index_v3::format::{
    FormatError, EXACT_IDX_MAGIC, EXACT_IDX_VERSION_V3, HEADER_SIZE, INDEX_ENTRY_SIZE,
};
use bittice::core::storage::exact_index_v3::reader::SnapshotReader;
use bittice::core::storage::exact_index_v3::writer::write_exact_index_v3;

// ── Helper: NFC canonical bytes (mirrors ExactIndex::hash internals) ─────────

fn canonical_bytes(s: &str) -> Vec<u8> {
    use unicode_normalization::{is_nfc, UnicodeNormalization};
    if is_nfc(s) {
        s.as_bytes().to_vec()
    } else {
        s.nfc().collect::<String>().into_bytes()
    }
}

fn hash_value(s: &str) -> u128 {
    xxh3_128(&canonical_bytes(s))
}

// ── Helper: make a RoaringBitmap with given values ────────────────────────────

fn bm(values: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &v in values {
        b.insert(v);
    }
    b
}

// ── Helper: make sorted entries (sorted by hash) ──────────────────────────────

fn sorted_entries(
    pairs: Vec<(u128, Vec<(u64, RoaringBitmap)>)>,
) -> Vec<(u128, Vec<(u64, RoaringBitmap)>)> {
    let mut v = pairs;
    v.sort_unstable_by_key(|(h, _)| *h);
    v
}

// =============================================================================
// (a) v3_writer_reader_roundtrip_small — 10 entries
// =============================================================================

#[test]
fn v3_writer_reader_roundtrip_small() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("small.idx");

    let n = 10usize;
    let entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = sorted_entries(
        (0..n)
            .map(|i| {
                let hash = hash_value(&format!("value_{}", i));
                let bitmaps = vec![(i as u64, bm(&[i as u32, i as u32 + 100]))];
                (hash, bitmaps)
            })
            .collect(),
    );

    write_exact_index_v3(&path, entries.clone()).expect("write");

    let reader = SnapshotReader::open(&path).expect("open");
    assert_eq!(reader.num_entries(), n as u64);

    // Verify all entries recoverable by get().
    for (hash, expected_bitmaps) in &entries {
        let got = reader.get(*hash).expect("get").expect("should exist");
        assert_eq!(
            got, *expected_bitmaps,
            "bitmap mismatch for hash 0x{:032x}",
            hash
        );
    }

    // Missing hash returns None.
    assert!(reader.get(u128::MAX).expect("get max").is_none());
}

// =============================================================================
// (b) v3_writer_reader_roundtrip_large — 10k entries
// =============================================================================

#[test]
fn v3_writer_reader_roundtrip_large() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("large.idx");

    let n = 10_000usize;
    let entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = sorted_entries(
        (0..n)
            .map(|i| {
                let hash = hash_value(&format!("email_{}@example.com", i));
                let bitmaps = vec![(i as u64, bm(&[i as u32]))];
                (hash, bitmaps)
            })
            .collect(),
    );

    write_exact_index_v3(&path, entries.clone()).expect("write");

    let reader = SnapshotReader::open(&path).expect("open");
    assert_eq!(reader.num_entries(), n as u64);

    // Check file size: 40 (header) + ? (bitmaps) + 28*N (index)
    let file_size = std::fs::metadata(&path).unwrap().len();
    // Index section alone must be at least 28 * N bytes.
    assert!(
        file_size >= 40 + (n as u64) * INDEX_ENTRY_SIZE as u64,
        "file_size {} is less than minimum expected",
        file_size
    );

    // Verify all entries recoverable.
    for (hash, expected_bitmaps) in &entries {
        let got = reader.get(*hash).expect("get").expect("should exist");
        assert_eq!(got, *expected_bitmaps);
    }
}

// =============================================================================
// (c) v3_get_missing_returns_none
// =============================================================================

#[test]
fn v3_get_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("missing.idx");

    let entries = sorted_entries(vec![
        (hash_value("present"), vec![(1u64, bm(&[1, 2, 3]))]),
    ]);
    write_exact_index_v3(&path, entries).expect("write");

    let reader = SnapshotReader::open(&path).expect("open");

    // These hashes are not in the file.
    assert!(reader.get(0).expect("get 0").is_none());
    assert!(reader.get(u128::MAX).expect("get max").is_none());
    assert!(reader.get(hash_value("absent")).expect("get absent").is_none());
}

// =============================================================================
// (d) v3_binary_search_boundaries
//     Entries at hashes 10, 20, 30, ..., 100.
//     Test first/last/mid/below/above/between.
// =============================================================================

#[test]
fn v3_binary_search_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("boundaries.idx");

    // Use direct u128 hashes (not hash_value) so we control the exact values.
    let entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = (1u128..=10)
        .map(|i| {
            let hash = i * 10;
            let bitmaps = vec![(i as u64, bm(&[i as u32]))];
            (hash, bitmaps)
        })
        .collect();

    // Already sorted (10, 20, ..., 100).
    write_exact_index_v3(&path, entries.clone()).expect("write");

    let reader = SnapshotReader::open(&path).expect("open");
    assert_eq!(reader.num_entries(), 10);

    // First entry (hash = 10).
    let r = reader.get(10).expect("get 10").expect("should exist");
    assert_eq!(r[0].0, 1u64, "seg_id for hash=10 should be 1");

    // Last entry (hash = 100).
    let r = reader.get(100).expect("get 100").expect("should exist");
    assert_eq!(r[0].0, 10u64, "seg_id for hash=100 should be 10");

    // Mid entry (hash = 50).
    let r = reader.get(50).expect("get 50").expect("should exist");
    assert_eq!(r[0].0, 5u64, "seg_id for hash=50 should be 5");

    // Below minimum (hash = 5) → None.
    assert!(reader.get(5).expect("get 5").is_none());

    // Above maximum (hash = 110) → None.
    assert!(reader.get(110).expect("get 110").is_none());

    // Between entries (hash = 15) → None.
    assert!(reader.get(15).expect("get 15").is_none());
}

// =============================================================================
// (e) v3_iter_entries — 100 entries, verify order and count
// =============================================================================

#[test]
fn v3_iter_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("iter.idx");

    let n = 100usize;
    let entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = sorted_entries(
        (0..n)
            .map(|i| {
                let hash = hash_value(&format!("iter_val_{}", i));
                let bitmaps = vec![(i as u64, bm(&[i as u32]))];
                (hash, bitmaps)
            })
            .collect(),
    );

    write_exact_index_v3(&path, entries.clone()).expect("write");

    let reader = SnapshotReader::open(&path).expect("open");

    let mut count = 0usize;
    let mut prev_hash: Option<u128> = None;
    for result in reader.iter_entries() {
        let (hash, _bitmaps) = result.expect("iter_entries should not error");
        if let Some(prev) = prev_hash {
            assert!(
                hash > prev,
                "iter_entries must yield entries in ascending hash order: prev=0x{:032x} cur=0x{:032x}",
                prev, hash
            );
        }
        prev_hash = Some(hash);
        count += 1;
    }

    assert_eq!(count, n, "iter_entries must yield exactly {} entries", n);
}

// =============================================================================
// (f) v3_writer_unsorted_input — panics in debug builds on unsorted input
// =============================================================================

#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn v3_writer_unsorted_input() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("unsorted.idx");

    // Deliberately unsorted: higher hash first.
    let entries = vec![
        (200u128, vec![(1u64, bm(&[1]))]),
        (100u128, vec![(2u64, bm(&[2]))]), // out of order
    ];

    write_exact_index_v3(&path, entries).expect("write");
}

// =============================================================================
// (g) v3_corrupted_header_too_short
// =============================================================================

#[test]
fn v3_corrupted_header_too_short() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("too_short.idx");

    // Write a file that's only 20 bytes (less than HEADER_SIZE=40).
    std::fs::write(&path, &[0u8; 20]).unwrap();

    let result = SnapshotReader::open(&path);
    assert!(result.is_err(), "short file must produce an error");
    match result.unwrap_err() {
        FormatError::TooShort { expected, actual } => {
            assert_eq!(expected, HEADER_SIZE, "expected {} bytes", HEADER_SIZE);
            assert_eq!(actual, 20);
        }
        other => panic!("expected TooShort, got: {}", other),
    }
}

// =============================================================================
// (h) v3_corrupted_bad_magic
// =============================================================================

#[test]
fn v3_corrupted_bad_magic() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad_magic.idx");

    // Write a 40-byte file with bad magic.
    let mut buf = [0u8; 40];
    buf[0..4].copy_from_slice(b"WXYZ"); // wrong magic
    buf[4] = EXACT_IDX_VERSION_V3;
    std::fs::write(&path, &buf).unwrap();

    let result = SnapshotReader::open(&path);
    assert!(result.is_err());
    match result.unwrap_err() {
        FormatError::BadMagic { found } => {
            assert_eq!(&found, b"WXYZ");
        }
        other => panic!("expected BadMagic, got: {}", other),
    }
}

// =============================================================================
// (i) v3_corrupted_invalid_num_entries
// =============================================================================

#[test]
fn v3_corrupted_invalid_num_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad_num_entries.idx");

    // Write a file with valid magic/version but num_entries that doesn't
    // match the file size.
    let mut buf = [0u8; 40];
    buf[0..4].copy_from_slice(&EXACT_IDX_MAGIC);
    buf[4] = EXACT_IDX_VERSION_V3;
    // num_entries = 1000 (but there's no data)
    buf[8..16].copy_from_slice(&1000u64.to_le_bytes());
    // bitmaps_section_offset = 40
    buf[16..24].copy_from_slice(&40u64.to_le_bytes());
    // index_section_offset = 40 (empty bitmaps section)
    buf[24..32].copy_from_slice(&40u64.to_le_bytes());
    std::fs::write(&path, &buf).unwrap();

    let result = SnapshotReader::open(&path);
    assert!(result.is_err());
    match result.unwrap_err() {
        FormatError::InvalidNumEntries { value, file_size } => {
            assert_eq!(value, 1000, "num_entries should be 1000");
            assert_eq!(file_size, 40, "file_size should be 40");
        }
        other => panic!("expected InvalidNumEntries, got: {}", other),
    }
}

// =============================================================================
// (j) v3_migrate_from_v2 — create v2, migrate, verify with SnapshotReader
// =============================================================================

#[test]
fn v3_migrate_from_v2() {
    use bittice::core::storage::exact_index::ExactIndex;
    use bittice::core::storage::exact_index_v3::migrate::migrate_exact_index_to_v3;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("migrate_v2.idx");

    let mut map: HashMap<u128, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    map.insert(
        ExactIndex::hash("user@example.com"),
        vec![(1u64, bm(&[1, 2, 3]))],
    );
    map.insert(
        ExactIndex::hash("admin@example.com"),
        vec![(2u64, bm(&[10, 20]))],
    );
    map.insert(
        ExactIndex::hash("noreply@example.com"),
        vec![(3u64, bm(&[100]))],
    );
    {
        let mut file = BufWriter::new(std::fs::File::create(&path).unwrap());
        file.write_all(b"BTXI").unwrap();
        file.write_all(&[2u8, 0, 0, 0]).unwrap();
        bincode::serialize_into(&mut file, &map).unwrap();
        file.flush().unwrap();
    }

    // Verify it's v2 before migration.
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[..4], b"BTXI");
    assert_eq!(raw[4], 2, "must be v2 before migration");

    // Migrate to v3.
    let stats = migrate_exact_index_to_v3(&path).expect("migrate");
    assert!(!stats.already_v3);
    assert_eq!(stats.num_entries, 3);
    assert!(stats.old_size > 0);
    assert!(stats.new_size > 0);

    // Verify the file is now v3.
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[..4], b"BTXI");
    assert_eq!(raw[4], EXACT_IDX_VERSION_V3, "must be v3 after migration");

    // Open with SnapshotReader and verify data.
    let reader = SnapshotReader::open(&path).expect("open v3");
    assert_eq!(reader.num_entries(), 3);

    let user_hash = hash_value("user@example.com");
    let user_bitmaps = reader.get(user_hash).expect("get").expect("should exist");
    assert_eq!(user_bitmaps.len(), 1);
    assert_eq!(user_bitmaps[0].0, 1u64);
    assert!(user_bitmaps[0].1.contains(1));
    assert!(user_bitmaps[0].1.contains(2));
    assert!(user_bitmaps[0].1.contains(3));

    let admin_hash = hash_value("admin@example.com");
    let admin_bitmaps = reader.get(admin_hash).expect("get").expect("should exist");
    assert_eq!(admin_bitmaps.len(), 1);
    assert_eq!(admin_bitmaps[0].0, 2u64);
    assert!(admin_bitmaps[0].1.contains(10));
    assert!(admin_bitmaps[0].1.contains(20));
}

// =============================================================================
// (k) v3_migrate_from_v1_legacy — raw HashMap bincode, migrate, verify queries
// =============================================================================

#[test]
fn v3_migrate_from_v1_legacy() {
    use bittice::core::storage::exact_index_v3::migrate::migrate_exact_index_to_v3;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("migrate_legacy.idx");

    // Write a raw HashMap<String, _> bincode (legacy format, no BTXI header).
    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    raw.insert("product_a".to_string(), vec![(10u64, bm(&[1, 2, 3, 4]))]);
    raw.insert("product_b".to_string(), vec![(11u64, bm(&[5, 6]))]);
    raw.insert("product_c".to_string(), vec![(12u64, bm(&[7]))]);

    {
        let mut file = BufWriter::new(std::fs::File::create(&path).unwrap());
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    // Migrate to v3.
    let stats = migrate_exact_index_to_v3(&path).expect("migrate");
    assert!(!stats.already_v3);
    assert_eq!(stats.num_entries, 3);

    // Verify the file is now v3.
    let raw_bytes = std::fs::read(&path).unwrap();
    assert_eq!(&raw_bytes[..4], b"BTXI");
    assert_eq!(raw_bytes[4], EXACT_IDX_VERSION_V3);

    // Open with SnapshotReader and verify all original string values are queryable.
    let reader = SnapshotReader::open(&path).expect("open");

    for (value, expected_entries) in &raw {
        let h = hash_value(value);
        let got = reader.get(h).expect("get").expect("should exist");
        assert_eq!(
            got.len(),
            expected_entries.len(),
            "entry count mismatch for {:?}",
            value
        );
        // Verify segment IDs.
        for (expected_seg_id, expected_bm) in expected_entries {
            let matching = got.iter().find(|(s, _)| s == expected_seg_id);
            assert!(
                matching.is_some(),
                "seg_id {} not found for {:?}",
                expected_seg_id, value
            );
            assert_eq!(&matching.unwrap().1, expected_bm);
        }
    }
}

// =============================================================================
// (l) v3_migrate_nfc_collapse — v1 file with NFC+NFD entries, verify collapse
// =============================================================================

#[test]
fn v3_migrate_nfc_collapse() {
    use bittice::core::storage::exact_index_v3::migrate::migrate_exact_index_to_v3;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("migrate_nfc.idx");

    let cafe_nfc = "caf\u{00e9}"; // é precompuesto (U+00E9)
    let cafe_nfd = "cafe\u{0301}"; // e + combining acute accent (U+0301)

    // Both strings are canonically equivalent → same hash after NFC.
    assert_ne!(cafe_nfc, cafe_nfd, "strings must be distinct at byte level");
    assert_eq!(
        hash_value(cafe_nfc),
        hash_value(cafe_nfd),
        "must hash identically after NFC"
    );

    // Write legacy file with both NFC and NFD forms.
    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    raw.insert(cafe_nfc.to_string(), vec![(10u64, bm(&[1, 2]))]);
    raw.insert(cafe_nfd.to_string(), vec![(20u64, bm(&[3, 4]))]);

    {
        let mut file = BufWriter::new(std::fs::File::create(&path).unwrap());
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    // Migrate to v3.
    let stats = migrate_exact_index_to_v3(&path).expect("migrate");
    assert!(!stats.already_v3);
    // After NFC collapse, there should be only 1 entry.
    assert_eq!(
        stats.num_entries, 1,
        "NFC and NFD variants must collapse to 1 entry"
    );

    // Open with SnapshotReader.
    let reader = SnapshotReader::open(&path).expect("open");
    assert_eq!(reader.num_entries(), 1);

    // Query with NFC form.
    let h_nfc = hash_value(cafe_nfc);
    let entries = reader.get(h_nfc).expect("get").expect("collapsed entry must exist");

    // Both segment IDs (10 and 20) must be present.
    let seg_ids: Vec<u64> = entries.iter().map(|(s, _)| *s).collect();
    assert!(
        seg_ids.contains(&10u64),
        "segment 10 (from NFC entry) must be present"
    );
    assert!(
        seg_ids.contains(&20u64),
        "segment 20 (from NFD entry) must be present"
    );

    // Rows 1, 2 from seg 10 and rows 3, 4 from seg 20 must be present.
    let seg10 = entries.iter().find(|(s, _)| *s == 10).map(|(_, b)| b);
    let seg20 = entries.iter().find(|(s, _)| *s == 20).map(|(_, b)| b);

    let bm10 = seg10.expect("segment 10 bitmap must exist");
    assert!(bm10.contains(1) && bm10.contains(2), "seg 10 must have rows 1 and 2");

    let bm20 = seg20.expect("segment 20 bitmap must exist");
    assert!(bm20.contains(3) && bm20.contains(4), "seg 20 must have rows 3 and 4");

    // Also query with NFD form — must return the same entry.
    let h_nfd = hash_value(cafe_nfd);
    assert_eq!(h_nfc, h_nfd, "hashes must be equal");
    let entries_nfd = reader.get(h_nfd).expect("get nfd").expect("must exist");
    assert_eq!(entries, entries_nfd, "NFC and NFD queries must return identical data");
}

// =============================================================================
// (m) v3_write_empty — empty snapshot, verify valid file with 0 entries
// =============================================================================

#[test]
fn v3_write_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("empty.idx");

    write_exact_index_v3(&path, std::iter::empty::<(u128, Vec<(u64, RoaringBitmap)>)>())
        .expect("write empty");

    // File should exist and be valid.
    let file_size = std::fs::metadata(&path).unwrap().len();

    // Minimum size: 40-byte header, 0 bitmaps, 0 index entries.
    assert_eq!(file_size, HEADER_SIZE as u64, "empty file must be exactly 40 bytes");

    // Magic and version must be correct.
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[0..4], &EXACT_IDX_MAGIC);
    assert_eq!(raw[4], EXACT_IDX_VERSION_V3);

    // num_entries must be 0.
    let num_entries = u64::from_le_bytes(raw[8..16].try_into().unwrap());
    assert_eq!(num_entries, 0);

    // SnapshotReader must open successfully.
    let reader = SnapshotReader::open(&path).expect("open empty");
    assert_eq!(reader.num_entries(), 0);
}

// =============================================================================
// (n) v3_get_after_write_empty — get on empty snapshot returns Ok(None)
// =============================================================================

#[test]
fn v3_get_after_write_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("empty_get.idx");

    write_exact_index_v3(&path, std::iter::empty::<(u128, Vec<(u64, RoaringBitmap)>)>())
        .expect("write empty");

    let reader = SnapshotReader::open(&path).expect("open");

    // Any hash must return Ok(None).
    assert!(reader.get(0).expect("get 0").is_none());
    assert!(reader.get(1).expect("get 1").is_none());
    assert!(reader.get(u128::MAX).expect("get max").is_none());
    assert!(reader.get(hash_value("anything")).expect("get anything").is_none());
}
