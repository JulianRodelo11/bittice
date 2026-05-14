//! Cross-protocol NFC test (deferred from Fase 1c).
//!
//! This test validates that inserting with one Unicode normalization form
//! and querying with another produces the correct result. Both REST and
//! gRPC ultimately call `Table::insert` and `Table::search`, so this test
//! covers the shared logic.
//!
//! NOTE: Transport-level coverage (REST handler + gRPC handler) requires
//! manual end-to-end testing with a running server. This test validates
//! the storage layer behavior that both protocols depend on.

use std::collections::HashMap;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

fn open_test_table(
    dir: &std::path::Path,
    name: &str,
    pk_field: &str,
) -> anyhow::Result<Table> {
    let mut table = Table::open(dir, name)?;
    table.manifest.primary_key = pk_field.to_string();
    table.manifest.original_fields = vec![
        pk_field.to_string(),
        "Name".to_string(),
        "Value".to_string(),
    ];
    Ok(table)
}

fn insert_row(table: &mut Table, pk: &str, name: &str, value: &str) {
    let pk_field = table.manifest.primary_key.clone();
    let mut row = HashMap::new();
    row.insert(pk_field, pk.to_string());
    row.insert("Name".to_string(), name.to_string());
    row.insert("Value".to_string(), value.to_string());
    table.insert(row).expect("insert");
}

fn query_by_pk(table: &Table, pk: &str) -> Option<(String, String)> {
    let pk_field = table.manifest.primary_key.clone();
    let fields = vec![pk_field.clone(), "Name".to_string(), "Value".to_string()];
    let filters = vec![Filter {
        field: pk_field,
        op: ComparisonOp::Eq,
        value: pk.to_string(),
        value_to: None,
        field_type: None,
        value_options: vec![],
    }];
    let result = table
        .search(&fields, &filters, &LogicalOp::And, &[], &[], 100, 0, None)
        .expect("search");
    result.rows.first().map(|row| {
        (
            row.get(1).cloned().unwrap_or_default(),
            row.get(2).cloned().unwrap_or_default(),
        )
    })
}

// ===========================================================================
// Insert NFD → Query NFC (simulates REST insert, gRPC query)
// ===========================================================================

#[test]
fn cross_protocol_insert_nfd_query_nfc() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Insert with NFD form (e.g., REST client sends decomposed é)
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "cafe\u{0301}", "nfd_insert", "val");
        table.flush_active_segment_buffers().expect("flush");
        table.close().expect("close");
    }

    // Query with NFC form (e.g., gRPC client sends precomposed é)
    {
        let table = Table::open(&dir, "t").expect("reopen");
        let result = query_by_pk(&table, "caf\u{00e9}");
        assert!(
            result.is_some(),
            "NFC query should find NFD-inserted row (canonical equivalence)"
        );
        assert_eq!(result.unwrap().0, "nfd_insert");
    }
}

// ===========================================================================
// Insert NFC → Query NFD (reverse direction)
// ===========================================================================

#[test]
fn cross_protocol_insert_nfc_query_nfd() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Insert with NFC form
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "caf\u{00e9}", "nfc_insert", "val");
        table.flush_active_segment_buffers().expect("flush");
        table.close().expect("close");
    }

    // Query with NFD form
    {
        let table = Table::open(&dir, "t").expect("reopen");
        let result = query_by_pk(&table, "cafe\u{0301}");
        assert!(
            result.is_some(),
            "NFD query should find NFC-inserted row (canonical equivalence)"
        );
        assert_eq!(result.unwrap().0, "nfc_insert");
    }
}

// ===========================================================================
// Multiple NFC/NFD variants all resolve to same row
// ===========================================================================

#[test]
fn cross_protocol_multiple_nfc_forms() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Various canonically equivalent forms of "ñ"
    let nfc_n = "\u{00f1}";       // ñ precompuesto
    let nfd_n = "n\u{0303}";      // ñ descompuesto

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, nfc_n, "n_tilde", "val");
        table.flush_active_segment_buffers().expect("flush");
        table.close().expect("close");
    }

    {
        let table = Table::open(&dir, "t").expect("reopen");
        // Both forms should find the same row
        let r1 = query_by_pk(&table, nfc_n);
        let r2 = query_by_pk(&table, nfd_n);
        assert!(r1.is_some(), "NFC form should find the row");
        assert!(r2.is_some(), "NFD form should find the same row");
        assert_eq!(r1.unwrap().0, r2.unwrap().0);
    }
}

// ===========================================================================
// After close + reopen, NFC normalization persists
// ===========================================================================

#[test]
fn cross_protocol_nfc_persists_across_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Insert, close, reopen multiple times
    for i in 0..3 {
        let mut table = Table::open(&dir, "t").expect("open");
        table.manifest.primary_key = "PK".to_string();
        if table.manifest.original_fields.is_empty() {
            table.manifest.original_fields = vec![
                "PK".to_string(),
                "Name".to_string(),
                "Value".to_string(),
            ];
        }
        insert_row(&mut table, "caf\u{00e9}", &format!("iter_{}", i), "val");
        table.close().expect("close");
    }

    // Query with NFD — should always work regardless of how many reopens
    let table = Table::open(&dir, "t").expect("reopen");
    let result = query_by_pk(&table, "cafe\u{0301}");
    assert!(result.is_some(), "NFD query should work after multiple reopens");
    // Last insert wins
    assert_eq!(result.unwrap().0, "iter_2");
}
