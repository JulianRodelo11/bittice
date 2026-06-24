//! Regression: `reconcile_orphan_rows` must not tombstone unrelated rows in the
//! active segment when PK columns are read via mmap before BufWriter flush.
//!
//! CDC UPDATE does delete + insert + reconcile on every event. Without flushing
//! the active segment first, mmap reads return empty PK strings; reconcile then
//! groups distinct rows under PK "" and keeps only the highest local_id.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

fn open_test_table(dir: &Path, table_name: &str, pk_field: &str) -> Result<Table> {
    let mut table = Table::open(dir, table_name)?;
    table.manifest.primary_key = pk_field.to_string();
    table.manifest.primary_key_columns = vec![pk_field.to_string()];
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
fn update_reconcile_keeps_other_pks_in_dirty_active_segment() {
    let dir = TempDir::new().expect("tmpdir");
    let mut table = open_test_table(dir.path(), "pagos", "pagoId").expect("open");

    table
        .insert(row("pagoId", "100", "A", "1"))
        .expect("insert 100");
    table
        .insert(row("pagoId", "200", "B", "2"))
        .expect("insert 200");
    assert_eq!(live_row_count(&mut table, "pagoId"), 2);

    // Mirrors CDC UPDATE: tombstone+append for one PK, then reconcile on dirty active seg.
    table
        .update("100", row("pagoId", "100", "A-updated", "9"))
        .expect("update 100");

    assert_eq!(
        live_row_count(&mut table, "pagoId"),
        2,
        "reconcile must not drop sibling PK still live in active segment"
    );

    let fields = vec![
        "pagoId".to_string(),
        "Name".to_string(),
        "Value".to_string(),
    ];
    table.flush_active_segment_buffers().expect("flush");
    let r200 = table
        .search(
            &fields,
            &pk_eq("pagoId", "200"),
            &LogicalOp::And,
            &[],
            &[],
            10,
            0,
            None,
        )
        .expect("search 200");
    assert_eq!(r200.total_found, 1, "PK 200 must survive UPDATE on PK 100");
    assert_eq!(r200.rows[0][2], "2");
}
