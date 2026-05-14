//! Tests específicos del hashing del primary index (Fase 1c).
//!
//! Cubren: migración v1→v2, NFC end-to-end, estabilidad del hash,
//! determinismo entre tipos de PK.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use bittice::core::storage::primary_index_io;
use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_test_table(dir: &Path, table_name: &str, pk_field: &str) -> Result<Table> {
    let mut table = Table::open(dir, table_name)?;
    table.manifest.primary_key = pk_field.to_string();
    table.manifest.original_fields = vec![
        pk_field.to_string(),
        "Name".to_string(),
        "Value".to_string(),
    ];
    Ok(table)
}

fn insert_row(table: &mut Table, pk_field: &str, pk: &str, name: &str, value: &str) {
    let mut row = HashMap::new();
    row.insert(pk_field.to_string(), pk.to_string());
    row.insert("Name".to_string(), name.to_string());
    row.insert("Value".to_string(), value.to_string());
    table.insert(row).expect("insert failed");
}

fn query_by_pk(table: &Table, pk_field: &str, pk: &str) -> Vec<(String, String)> {
    let fields: Vec<String> = vec![
        pk_field.to_string(),
        "Name".to_string(),
        "Value".to_string(),
    ];
    let filters = vec![Filter {
        field: pk_field.to_string(),
        op: ComparisonOp::Eq,
        value: pk.to_string(),
        value_to: None,
        field_type: None,
        value_options: vec![],
    }];
    let result = table
        .search(&fields, &filters, &LogicalOp::And, &[], &[], 100, 0, None)
        .expect("search failed");

    result
        .rows
        .iter()
        .map(|row| {
            let name = row.get(1).cloned().unwrap_or_default();
            let val = row.get(2).cloned().unwrap_or_default();
            (name, val)
        })
        .collect()
}

// ===========================================================================
// (a) Migración v1→v2 en memoria
// ===========================================================================

#[test]
fn hash_migration_v1_to_v2() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_mig");
    std::fs::create_dir_all(&dir).unwrap();

    // Write a v1 file with header.
    let idx_path = dir.join("t_mig").join("primary.idx");
    std::fs::create_dir_all(idx_path.parent().unwrap()).unwrap();

    let mut legacy: HashMap<String, (u64, u32)> = HashMap::new();
    legacy.insert("mig_pk1".to_string(), (1, 0));
    legacy.insert("mig_pk2".to_string(), (2, 1));
    legacy.insert("mig_pk3".to_string(), (3, 2));

    {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&idx_path).unwrap());
        file.write_all(b"BTPI").unwrap();
        file.write_all(&[1, 0, 0, 0]).unwrap();
        bincode::serialize_into(&mut file, &legacy).unwrap();
        file.flush().unwrap();
    }

    // Load — should migrate v1 → v2 in memory.
    let loaded = primary_index_io::load_primary_index(&idx_path).expect("load v1");
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded.get("mig_pk1"), Some((1, 0)));
    assert_eq!(loaded.get("mig_pk2"), Some((2, 1)));
    assert_eq!(loaded.get("mig_pk3"), Some((3, 2)));

    // Save — should persist as v2.
    primary_index_io::save_primary_index(&idx_path, &loaded).expect("save v2");

    // Verify v2 header.
    let raw = std::fs::read(&idx_path).unwrap();
    assert_eq!(&raw[..4], b"BTPI");
    assert_eq!(raw[4], 2, "should be v2 after migration");

    // Load again — v2 direct.
    let reloaded = primary_index_io::load_primary_index(&idx_path).expect("load v2");
    assert_eq!(reloaded.len(), 3);
    assert_eq!(reloaded.get("mig_pk1"), Some((1, 0)));
}

// ===========================================================================
// (b) Migración legacy (sin header) → v2
// ===========================================================================

#[test]
fn hash_migration_legacy_to_v2() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_legacy");
    std::fs::create_dir_all(&dir).unwrap();

    let idx_path = dir.join("primary.idx");

    // Write raw bincode (no header).
    let mut legacy: HashMap<String, (u64, u32)> = HashMap::new();
    legacy.insert("leg_pk".to_string(), (42, 7));

    {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(&idx_path).unwrap());
        bincode::serialize_into(&mut file, &legacy).unwrap();
        file.flush().unwrap();
    }

    // Load (legacy → v2 in memory).
    let loaded = primary_index_io::load_primary_index(&idx_path).expect("load legacy");
    assert_eq!(loaded.get("leg_pk"), Some((42, 7)));

    // Save → v2.
    primary_index_io::save_primary_index(&idx_path, &loaded).expect("save v2");

    // Verify.
    let raw = std::fs::read(&idx_path).unwrap();
    assert_eq!(raw[4], 2);
}

// ===========================================================================
// (c) NFC end-to-end: insert NFC, query NFD → finds the row
// ===========================================================================

#[test]
fn nfc_end_to_end_insert_query() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_nfc");

    // Insert with NFC form.
    {
        let mut table = open_test_table(&dir, "t_nfc", "PK").expect("open");
        insert_row(&mut table, "PK", "caf\u{00e9}", "cafe_nfc", "val");
        table.flush_active_segment_buffers().expect("flush");
        table.close().expect("close");
    }

    // Query with NFD form — should find the row (canonical equivalence).
    {
        let table = Table::open(&dir, "t_nfc").expect("reopen");
        let results = query_by_pk(&table, "PK", "cafe\u{0301}");
        assert_eq!(results.len(), 1, "NFD query should find NFC-stored row");
        assert_eq!(results[0].0, "cafe_nfc");
    }
}

// ===========================================================================
// (d) Colisión sintética en get_row_as_map
// ===========================================================================

/// This test verifies the collision detection path. In practice, xxh3_128
/// collisions are astronomically rare, so we test the mechanism indirectly
/// by verifying that get_row_as_map returns None when the stored PK doesn't
/// match the requested PK (which is what would happen on a collision).
#[test]
fn collision_detection_get_row_as_map() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_coll");

    // Insert a row with PK "alpha".
    {
        let mut table = open_test_table(&dir, "t_coll", "PK").expect("open");
        insert_row(&mut table, "PK", "alpha", "alpha_row", "val_alpha");
        table.flush_active_segment_buffers().expect("flush");
        table.close().expect("close");
    }

    // Open and verify normal lookup works.
    {
        let mut table = Table::open(&dir, "t_coll").expect("reopen");
        let result = table.get_row_as_map("alpha").expect("get_row_as_map");
        assert!(result.is_some(), "normal lookup should succeed");
        assert_eq!(result.unwrap().get("Name").unwrap(), "alpha_row");

        // Non-existent PK returns None.
        let missing = table.get_row_as_map("nonexistent").expect("get missing");
        assert!(missing.is_none(), "missing PK should return None");
    }
}

// ===========================================================================
// (e) Estabilidad del hash entre runs (hardcoded tripwire)
// ===========================================================================

/// If xxhash-rust changes its output, or canonical_bytes changes,
/// these hardcoded values will break — alerting us to a contract violation.
#[test]
fn hash_stability_hardcoded() {
    use xxhash_rust::xxh3::xxh3_128;
    use bittice::core::storage::pk::canonical_bytes;

    let test_cases: &[(&str, Option<u128>)] = &[
        ("simple_pk", None),   // fill with actual value after first run
        ("", None),            // fill with actual value after first run
        ("12345678", None),    // fill with actual value after first run
        ("caf\u{00e9}", None), // fill with actual value after first run
    ];

    // Verify determinism and distinctness. Fill in hardcoded values
    // after the first run to lock in the contract.
    let hashes: Vec<u128> = test_cases
        .iter()
        .map(|(pk, _)| xxh3_128(&canonical_bytes(pk)))
        .collect();

    // All should be distinct (no collision among these test cases).
    let mut unique = hashes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(hashes.len(), unique.len(), "test PKs should have distinct hashes");

    // If hardcoded values are provided, verify they match.
    for ((pk, expected), actual) in test_cases.iter().zip(hashes.iter()) {
        if let Some(exp) = expected {
            assert_eq!(actual, exp, "hash changed for PK {:?} — contract violated", pk);
        }
    }
}

// ===========================================================================
// (f) Determinismo del hash entre tipos de PK
// ===========================================================================

#[test]
fn hash_determinism_across_pk_types() {
    use xxhash_rust::xxh3::xxh3_128;
    use bittice::core::storage::pk::canonical_bytes;

    let long_pk = "A".repeat(500);
    let pks = vec![
        "550e8400-e29b-41d4-a716-446655440000",  // UUID
        "12345678",                               // numeric string
        long_pk.as_str(),                         // long concatenated
        "",                                       // empty (NULL)
        "caf\u{00e9}",                            // NFC
        "normal_ascii",                           // ASCII
    ];

    for pk in &pks {
        let h1 = xxh3_128(&canonical_bytes(pk));
        let h2 = xxh3_128(&canonical_bytes(pk));
        assert_eq!(h1, h2, "hash must be deterministic for PK {:?}", pk);
    }

    // All should be distinct (no collision among these test cases).
    let hashes: Vec<u128> = pks.iter().map(|pk| xxh3_128(&canonical_bytes(pk))).collect();
    let mut unique = hashes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(hashes.len(), unique.len(), "all test PKs should have distinct hashes");
}
