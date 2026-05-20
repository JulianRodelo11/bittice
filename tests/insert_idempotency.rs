//! Idempotency invariants on `Table::insert`.
//!
//! These cover the architectural fix that closes the ghost-row class of
//! bugs: re-delivering a binlog event (or re-playing a WAL entry) for a PK
//! that already exists in the mirror must NOT produce a second physical
//! row. Specifically:
//!
//!   1. Re-inserting the same (pk, row) → no-op. Count stays at 1, query
//!      still returns the original.
//!   2. Re-inserting the same pk with DIFFERENT row → behaves as UPDATE.
//!      Old row is tombstoned, new row is appended. Live count stays at 1
//!      and the query returns the new content.
//!   3. After many idempotent re-applies, the live row count for the table
//!      equals the number of distinct PKs (no accumulated duplicates).
//!
//! These mirror exactly what CDC replay does after a crash where
//! `cdc_state.json` lagged behind durable mirror writes.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

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

fn row(pk_field: &str, pk: &str, name: &str, value: &str) -> HashMap<String, String> {
    let mut r = HashMap::new();
    r.insert(pk_field.to_string(), pk.to_string());
    r.insert("Name".to_string(), name.to_string());
    r.insert("Value".to_string(), value.to_string());
    r
}

fn pk_eq(pk_field: &str, value: &str) -> Vec<Filter> {
    vec![Filter {
        field: pk_field.to_string(),
        op: ComparisonOp::Eq,
        value: value.to_string(),
        value_to: None,
        field_type: None,
        value_options: vec![],
    }]
}

fn live_row_count(table: &mut Table, pk_field: &str) -> usize {
    table.flush_active_segment_buffers().expect("flush");
    let fields: Vec<String> = vec![pk_field.to_string(), "Name".to_string(), "Value".to_string()];
    table
        .search(
            &fields,
            &[],
            &LogicalOp::And,
            &[],
            &[],
            10_000,
            0,
            None,
        )
        .expect("search")
        .rows
        .len()
}

#[test]
fn reinserting_same_row_is_a_noop() {
    let dir = TempDir::new().expect("tmpdir");
    let mut table = open_test_table(dir.path(), "t", "id").expect("open");

    table.insert(row("id", "1", "alice", "10")).expect("insert 1");
    table.insert(row("id", "1", "alice", "10")).expect("insert 1 again");
    table.insert(row("id", "1", "alice", "10")).expect("insert 1 again");

    assert_eq!(live_row_count(&mut table, "id"), 1, "duplicate inserts must collapse");
}

#[test]
fn reinserting_same_pk_with_new_content_replaces_old() {
    let dir = TempDir::new().expect("tmpdir");
    let mut table = open_test_table(dir.path(), "t", "id").expect("open");

    table.insert(row("id", "1", "alice", "10")).expect("insert");
    table.insert(row("id", "1", "alice", "20")).expect("update via insert");

    assert_eq!(live_row_count(&mut table, "id"), 1, "no ghost on replace");

    table.flush_active_segment_buffers().expect("flush");
    let fields: Vec<String> = vec!["id".to_string(), "Name".to_string(), "Value".to_string()];
    let res = table
        .search(
            &fields,
            &pk_eq("id", "1"),
            &LogicalOp::And,
            &[],
            &[],
            10,
            0,
            None,
        )
        .expect("search");
    assert_eq!(res.rows.len(), 1);
    // The 3rd column ("Value") should be "20" — the latest write.
    assert_eq!(res.rows[0][2], "20");
}

#[test]
fn many_replays_dont_accumulate_duplicates() {
    let dir = TempDir::new().expect("tmpdir");
    let mut table = open_test_table(dir.path(), "t", "id").expect("open");

    // 50 distinct PKs, each inserted exactly once.
    for i in 0..50 {
        table
            .insert(row("id", &i.to_string(), "n", "v"))
            .expect("first insert");
    }

    // Replay each row 5 more times — simulates CDC re-delivering all 50
    // events because state lagged behind.
    for _ in 0..5 {
        for i in 0..50 {
            table
                .insert(row("id", &i.to_string(), "n", "v"))
                .expect("replay insert");
        }
    }

    assert_eq!(
        live_row_count(&mut table, "id"),
        50,
        "300 inserts of 50 distinct PKs must yield 50 live rows"
    );
}
