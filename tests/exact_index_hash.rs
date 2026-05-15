//! Phase 1c tests for the hash-keyed exact index.
//!
//! Covers:
//! - NFC end-to-end (moved from exact_index_baseline after Phase 1c)
//! - V1→v2 migration via load_exact_index
//! - Legacy (no-header) migration
//! - NFC collapse: two NFD/NFC variants merge into one entry
//! - Hash stability (hardcoded tripwire values)
//! - Determinism and NFC/NFD equivalence
//! - Build + query roundtrip (v2 .idx file, 100 emails)

use anyhow::Result;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};
use xxhash_rust::xxh3::xxh3_128;

// ---------------------------------------------------------------------------
// Helper: NFC bytes (mirrors exact_index::ExactIndex::hash internals)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Helpers: table manipulation
// ---------------------------------------------------------------------------

fn open_table(dir: &Path, name: &str, pk: &str, fields: &[&str]) -> Result<Table> {
    let mut t = Table::open(dir, name)?;
    t.manifest.primary_key = pk.to_string();
    t.manifest.original_fields = fields.iter().map(|s| s.to_string()).collect();
    Ok(t)
}

fn insert(table: &mut Table, pk_field: &str, pk: &str, extra: &[(&str, &str)]) {
    let mut row = HashMap::new();
    row.insert(pk_field.to_string(), pk.to_string());
    for (k, v) in extra {
        row.insert(k.to_string(), v.to_string());
    }
    table.insert(row).expect("insert");
}

fn count_matches(table: &Table, fields: &[&str], filter_field: &str, value: &str) -> usize {
    let field_list: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    table
        .search(
            &field_list,
            &[Filter {
                field: filter_field.to_string(),
                op: ComparisonOp::Eq,
                value: value.to_string(),
                value_to: None,
                field_type: None,
                value_options: vec![],
            }],
            &LogicalOp::And,
            &[],
            &[],
            1000,
            0,
            None,
        )
        .expect("search")
        .total_found
}

// ===========================================================================
// Test (a): exact_index_nfc_end_to_end
//
// Previously exact_index_no_nfc_normalization_today in exact_index_baseline.rs
// with #[ignore].  Now that Phase 1c hashes via NFC, this must pass.
// ===========================================================================

#[test]
fn exact_index_nfc_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("nfc_e2e");

    let nfd_email = "cafe\u{0301}@example.com"; // NFD form
    let nfc_email = "caf\u{00e9}@example.com";   // NFC form (precomposed é)

    let mut t = open_table(&dir, "tbl", "PK", &["PK", "Email"]).unwrap();
    insert(&mut t, "PK", "pk1", &[("Email", nfd_email)]);
    // Rotate to immutable segment so exact index is built/persisted.
    t.flush_active_segment().unwrap();

    // Query with NFC form must find the row inserted with NFD form.
    let found = count_matches(&t, &["PK", "Email"], "Email", nfc_email);
    assert_eq!(
        found, 1,
        "NFC query must find row inserted with NFD value (Phase 1c: hash via xxh3_128(NFC(v)))"
    );

    // Also query with NFD form — must also find it.
    let found_nfd = count_matches(&t, &["PK", "Email"], "Email", nfd_email);
    assert_eq!(found_nfd, 1, "NFD query must also find the row");
}

// ===========================================================================
// Test (b): exact_index_migration_v1_to_v2
//
// Create a v1 file (BTXI + version 1 + bincode HashMap<String, _>).
// Load it, verify get() works, save, reload and verify v2 magic.
// ===========================================================================

#[test]
fn exact_index_migration_v1_to_v2() {
    use bittice::core::storage::exact_index_io::{load_exact_index, save_exact_index};
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("exact_v1.idx");

    // Build a v1 payload: BTXI + version 1 + bincode HashMap<String, Vec<(u64, Bitmap)>>
    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    let mut bm1 = RoaringBitmap::new();
    bm1.insert(5);
    bm1.insert(10);
    raw.insert("user@example.com".to_string(), vec![(1u64, bm1.clone())]);
    let mut bm2 = RoaringBitmap::new();
    bm2.insert(42);
    raw.insert("admin@example.com".to_string(), vec![(2u64, bm2.clone())]);

    {
        let mut file = BufWriter::new(fs::File::create(&path).unwrap());
        file.write_all(b"BTXI").unwrap();
        file.write_all(&[1u8, 0, 0, 0]).unwrap(); // version 1
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    // Load should succeed and migrate to v2 in memory.
    let idx = load_exact_index(&path).expect("load v1");
    assert_eq!(idx.len(), 2);

    let user_entries = idx.get("user@example.com").expect("user@example.com should exist");
    assert_eq!(user_entries.len(), 1);
    assert_eq!(user_entries[0].0, 1u64);
    assert_eq!(user_entries[0].1, bm1);

    let admin_entries = idx.get("admin@example.com").expect("admin@example.com should exist");
    assert_eq!(admin_entries.len(), 1);
    assert_eq!(admin_entries[0].0, 2u64);
    assert_eq!(admin_entries[0].1, bm2);

    // Save to a new path — must write v2.
    let path_v2 = tmp.path().join("exact_v2.idx");
    save_exact_index(&path_v2, &idx).expect("save v2");

    let raw_bytes = fs::read(&path_v2).unwrap();
    assert_eq!(&raw_bytes[..4], b"BTXI", "v2 magic must be BTXI");
    assert_eq!(raw_bytes[4], 2, "v2 version byte must be 2");

    // Reload v2 — data must still be correct.
    let reloaded = load_exact_index(&path_v2).expect("reload v2");
    assert_eq!(reloaded.len(), 2);
    assert!(reloaded.get("user@example.com").is_some());
    assert!(reloaded.get("admin@example.com").is_some());
}

// ===========================================================================
// Test (c): exact_index_migration_legacy_no_header
//
// Create a file with just bincode HashMap<String, _> (no BTXI header).
// Load, verify get() works, save produces v2 header.
// ===========================================================================

#[test]
fn exact_index_migration_legacy_no_header() {
    use bittice::core::storage::exact_index_io::{load_exact_index, save_exact_index};
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("exact_legacy.idx");

    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    let mut bm = RoaringBitmap::new();
    bm.insert(7);
    bm.insert(99);
    raw.insert("legacy_field".to_string(), vec![(3u64, bm.clone())]);

    {
        let mut file = BufWriter::new(fs::File::create(&path).unwrap());
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    // Load must succeed.
    let idx = load_exact_index(&path).expect("load legacy");
    assert_eq!(idx.len(), 1);
    let entries = idx.get("legacy_field").expect("legacy_field should exist");
    assert_eq!(entries[0].0, 3u64);
    assert_eq!(entries[0].1, bm);

    // Save must produce v2 header.
    save_exact_index(&path, &idx).expect("save");
    let raw_bytes = fs::read(&path).unwrap();
    assert_eq!(&raw_bytes[..4], b"BTXI", "after save: magic must be BTXI");
    assert_eq!(raw_bytes[4], 2, "after save: version must be 2");
}

// ===========================================================================
// Test (d): exact_index_migration_nfc_collapse
//
// Build a v1 file with two entries: "café" NFC and "café" NFD (same canonical
// form). After migration, len() == 1 and the bitmap contains IDs from both.
// ===========================================================================

#[test]
fn exact_index_migration_nfc_collapse() {
    use bittice::core::storage::exact_index_io::load_exact_index;
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("exact_nfc_collapse.idx");

    let cafe_nfc = "caf\u{00e9}"; // é precompuesto (U+00E9)
    let cafe_nfd = "cafe\u{0301}"; // e + combining acute accent (U+0301)

    // Both strings are canonically equivalent → same NFC bytes → same hash.
    assert_ne!(cafe_nfc, cafe_nfd, "strings must be distinct at byte level");
    assert_eq!(hash_value(cafe_nfc), hash_value(cafe_nfd), "must hash identically after NFC");

    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();

    let mut bm_nfc = RoaringBitmap::new();
    bm_nfc.insert(1);
    bm_nfc.insert(2);
    raw.insert(cafe_nfc.to_string(), vec![(10u64, bm_nfc)]);

    let mut bm_nfd = RoaringBitmap::new();
    bm_nfd.insert(3);
    bm_nfd.insert(4);
    raw.insert(cafe_nfd.to_string(), vec![(20u64, bm_nfd)]);

    // Write as legacy (no header) so load_exact_index triggers migration.
    {
        let mut file = BufWriter::new(fs::File::create(&path).unwrap());
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    let idx = load_exact_index(&path).expect("load");

    // Both entries collapse to the same hash → len == 1.
    assert_eq!(
        idx.len(), 1,
        "NFC and NFD variants must collapse to a single entry (len=1, got {})",
        idx.len()
    );

    // The single entry must be reachable by both NFC and NFD queries.
    let entries_nfc = idx.get(cafe_nfc).expect("NFC query must find the collapsed entry");
    let entries_nfd = idx.get(cafe_nfd).expect("NFD query must find the collapsed entry");

    // Both queries return the same entry.
    assert_eq!(entries_nfc.len(), entries_nfd.len());

    // Collect all segment IDs across all bitmaps.
    let all_seg_ids: Vec<u64> = entries_nfc.iter().map(|(seg_id, _)| *seg_id).collect();
    assert!(
        all_seg_ids.contains(&10u64),
        "segment 10 (from NFC entry) must be present"
    );
    assert!(
        all_seg_ids.contains(&20u64),
        "segment 20 (from NFD entry) must be present"
    );

    // Rows 1,2 from seg 10 and rows 3,4 from seg 20 must be present.
    let seg10 = entries_nfc.iter().find(|(s, _)| *s == 10).map(|(_, b)| b);
    let seg20 = entries_nfc.iter().find(|(s, _)| *s == 20).map(|(_, b)| b);

    let bm10 = seg10.expect("segment 10 bitmap must exist");
    assert!(bm10.contains(1) && bm10.contains(2), "seg 10 bitmap must have rows 1 and 2");

    let bm20 = seg20.expect("segment 20 bitmap must exist");
    assert!(bm20.contains(3) && bm20.contains(4), "seg 20 bitmap must have rows 3 and 4");
}

// ===========================================================================
// Test (e): exact_index_hash_stability_hardcoded
//
// Tripwire test — verifies the hash function doesn't silently change.
// Expected values computed once with xxhash-rust 0.8 + NFC normalization.
// ===========================================================================

#[test]
fn exact_index_hash_stability_hardcoded() {
    // These values were computed with xxhash-rust 0.8 xxh3_128 + NFC.
    // If any of these assertions fail, the hash function changed and all
    // existing v2 exact index files on disk are now inconsistent.
    let cases: &[(&str, u128)] = &[
        ("",                                          204254712233039002205064565430793619839u128),
        ("test@example.com",                          156600728645664085201031984443831390137u128),
        ("Jos\u{00e9}",                               14701072631514422380678635216088162908u128),  // NFC
        ("2025-03-15",                                273746748618928447527405682724450632392u128),
        ("550e8400-e29b-41d4-a716-446655440000",      169996977944382471549905116226226486165u128),
    ];

    for (value, expected) in cases {
        let got = hash_value(value);
        assert_eq!(
            got, *expected,
            "HASH STABILITY FAILURE for {:?}: expected 0x{:032x}, got 0x{:032x}. \
             The hash function changed — all v2 exact index files must be migrated!",
            value, expected, got
        );
    }
}

// ===========================================================================
// Test (f): exact_index_determinism
//
// For each value type, hash twice and verify same result.
// Also verify NFC/NFD forms produce the same hash.
// ===========================================================================

#[test]
fn exact_index_determinism() {
    let values: &[&str] = &[
        "550e8400-e29b-41d4-a716-446655440000", // UUID
        "user@example.com",                       // email
        "2025-03-15",                             // date
        "Jos\u{00e9} Gonz\u{00e1}lez",           // name with accents
        "42.0",                                   // number
        "",                                       // empty string
    ];

    for &v in values {
        let h1 = hash_value(v);
        let h2 = hash_value(v);
        assert_eq!(h1, h2, "hash of {:?} must be deterministic", v);
    }

    // NFC/NFD equivalence pairs.
    let nfc_nfd_pairs: &[(&str, &str)] = &[
        ("caf\u{00e9}",         "cafe\u{0301}"),        // é
        ("Jos\u{00e9}",         "Jose\u{0301}"),         // é in name
        ("\u{00f1}",            "n\u{0303}"),             // ñ
        ("\u{d55c}",            "\u{1112}\u{1161}\u{11ab}"), // 한 Hangul
    ];

    for (nfc, nfd) in nfc_nfd_pairs {
        assert_eq!(
            hash_value(nfc),
            hash_value(nfd),
            "NFC {:?} and NFD {:?} must produce the same hash",
            nfc, nfd
        );
    }
}

// ===========================================================================
// Test (g): exact_index_build_and_query_roundtrip
//
// Insert 100 rows with unique email values, flush, close, reopen.
// Verify the .idx file has BTXI v2 magic and all 100 emails are queryable.
// ===========================================================================

#[test]
fn exact_index_build_and_query_roundtrip() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ei_roundtrip");

    let emails: Vec<String> = (0..100).map(|i| format!("user{}@example.com", i)).collect();

    {
        let mut t = open_table(&dir, "tbl", "PK", &["PK", "Email"]).unwrap();
        for (i, email) in emails.iter().enumerate() {
            insert(&mut t, "PK", &format!("pk_{}", i), &[("Email", email.as_str())]);
        }
        // Rotate to immutable segment so exact index gets built and persisted.
        t.flush_active_segment().unwrap();
        t.close().unwrap();
    }

    // Verify .idx file has BTXI v2 magic.
    let idx_path = dir.join("tbl").join("secondary_exact").join("exact_Email.idx");
    assert!(
        idx_path.exists(),
        "exact_Email.idx must exist after close(), path: {:?}",
        idx_path
    );
    let raw = fs::read(&idx_path).unwrap();
    assert_eq!(&raw[..4], b"BTXI", "exact_Email.idx magic must be BTXI");
    assert_eq!(raw[4], 2, "exact_Email.idx version must be 2 (v2)");

    // Reopen and verify all 100 emails are queryable.
    let t = Table::open(&dir, "tbl").unwrap();
    for email in &emails {
        let found = count_matches(&t, &["PK", "Email"], "Email", email);
        assert_eq!(
            found, 1,
            "email {:?} must be findable after reopen, got {}",
            email, found
        );
    }
}
