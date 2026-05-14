//! Baseline integration tests for `compact()` behavior.
//!
//! These tests verify that compaction preserves all live rows and their
//! PK lookups. They must remain green throughout Fase 1c.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

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
// Test: compact() preserves all live rows
// ===========================================================================

#[test]
fn compact_preserves_all_rows() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_compact");

    let pks: Vec<String> = (0..200).map(|i| format!("compact_pk_{:04}", i)).collect();

    // Insert rows and create multiple immutable segments by flushing.
    {
        let mut table = open_test_table(&dir, "t_compact", "PK").expect("open");

        for (i, pk) in pks.iter().enumerate() {
            insert_row(
                &mut table,
                "PK",
                pk,
                &format!("name_{}", i),
                &format!("val_{}", i),
            );

            // Flush every 50 rows to create multiple segments.
            // flush_active_segment rotates the active segment into immutable.
            if (i + 1) % 50 == 0 {
                table.flush_active_segment().expect("flush");
            }
        }
        table.close().expect("close");
    }

    // Reopen and compact.
    {
        let mut table = Table::open(&dir, "t_compact").expect("reopen");

        // Verify we have multiple immutable segments (precondition for compact).
        let seg_count = table.immutable_segment_count();
        assert!(
            seg_count >= 4,
            "need >= 4 immutable segments for compact, got {}",
            seg_count
        );

        let compacted = table.compact().expect("compact");
        assert!(compacted > 0, "compact should have merged segments");

        // Verify all rows are still findable after compaction.
        for (i, pk) in pks.iter().enumerate() {
            let results = query_by_pk(&table, "PK", pk);
            assert_eq!(
                results.len(),
                1,
                "PK '{}' should be found after compact",
                pk
            );
            assert_eq!(results[0].0, format!("name_{}", i));
            assert_eq!(results[0].1, format!("val_{}", i));
        }
    }
}

// ===========================================================================
// Test: compact() with some deleted rows
// ===========================================================================

#[test]
fn compact_skips_deleted_rows() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_compact_del");

    let pks: Vec<String> = (0..200).map(|i| format!("del_pk_{:04}", i)).collect();

    // Insert rows, delete half, create segments.
    {
        let mut table = open_test_table(&dir, "t_compact_del", "PK").expect("open");

        for (i, pk) in pks.iter().enumerate() {
            insert_row(
                &mut table,
                "PK",
                pk,
                &format!("name_{}", i),
                &format!("val_{}", i),
            );
        }

        // Delete even-indexed rows.
        for (i, pk) in pks.iter().enumerate() {
            if i % 2 == 0 {
                table.delete(pk).expect("delete");
            }
        }

        // Flush to create immutable segments.
        table.flush_active_segment().expect("flush");

        // Create more segments by inserting more rows and flushing.
        for i in 200..250 {
            let pk = format!("extra_pk_{:04}", i);
            insert_row(&mut table, "PK", &pk, &format!("name_{}", i), &format!("val_{}", i));
        }
        table.flush_active_segment().expect("flush 2");

        for i in 250..300 {
            let pk = format!("extra_pk_{:04}", i);
            insert_row(&mut table, "PK", &pk, &format!("name_{}", i), &format!("val_{}", i));
        }
        table.flush_active_segment().expect("flush 3");

        table.close().expect("close");
    }

    // Reopen, compact, verify.
    {
        let mut table = Table::open(&dir, "t_compact_del").expect("reopen");

        let _ = table.compact().expect("compact");

        // Verify deleted rows are NOT findable.
        for (i, pk) in pks.iter().enumerate() {
            let results = query_by_pk(&table, "PK", pk);
            if i % 2 == 0 {
                assert_eq!(results.len(), 0, "deleted PK '{}' should not be found", pk);
            } else {
                assert_eq!(results.len(), 1, "live PK '{}' should be found", pk);
            }
        }

        // Verify extra rows are findable.
        for i in 200..300 {
            let pk = format!("extra_pk_{:04}", i);
            let results = query_by_pk(&table, "PK", &pk);
            assert_eq!(results.len(), 1, "extra PK '{}' should be found", pk);
        }
    }
}

// ===========================================================================
// Test: compact() with different PK types
// ===========================================================================

#[test]
fn compact_with_various_pk_types() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let dir = tmp.path().join("entity_compact_types");

    let pks = vec![
        "uuid-550e8400-e29b-41d4-a716-446655440000",
        "12345678",
        "long_pk_data_that_is_quite_large_and_should_still_work_after_compaction_1234567890",
        "",
        "caf\u{00e9}",          // NFC (NFD form hashes to same bucket — omit duplicate)
        "normal_ascii_pk",
    ];

    // Insert and create segments.
    {
        let mut table = open_test_table(&dir, "t_compact_types", "PK").expect("open");

        for (i, pk) in pks.iter().enumerate() {
            insert_row(&mut table, "PK", pk, &format!("name_{}", i), &format!("val_{}", i));
        }

        // Create extra segments by inserting more rows and flushing.
        for batch in 0..4 {
            for j in 0..50 {
                let pk = format!("batch_{}_row_{}", batch, j);
                insert_row(&mut table, "PK", &pk, &format!("n_{}_{}", batch, j), &format!("v_{}_{}", batch, j));
            }
            table.flush_active_segment().expect("flush");
        }

        table.close().expect("close");
    }

    // Reopen, compact, verify.
    {
        let mut table = Table::open(&dir, "t_compact_types").expect("reopen");
        let _ = table.compact().expect("compact");

        // Verify all original PKs are findable.
        for (i, pk) in pks.iter().enumerate() {
            let results = query_by_pk(&table, "PK", pk);
            assert_eq!(results.len(), 1, "PK '{}' should be found after compact", pk);
            assert_eq!(results[0].0, format!("name_{}", i));
        }
    }
}
