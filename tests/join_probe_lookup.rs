//! Probe join / exact-index point lookup (DuckDB-style narrow fetch).

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

fn open_table(dir: &Path, name: &str, pk: &str, fields: &[&str]) -> Result<Table> {
    let mut t = Table::open(dir, name)?;
    t.manifest.primary_key = pk.to_string();
    t.manifest.primary_key_columns = vec![pk.to_string()];
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

fn eq(field: &str, value: &str) -> Filter {
    Filter {
        field: field.to_string(),
        op: ComparisonOp::Eq,
        value: value.to_string(),
        value_to: None,
        field_type: None,
        value_options: vec![],
    }
}

#[test]
fn probe_fetch_matches_search_for_point_lookup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut table = open_table(dir.path(), "veh", "id", &["id", "Cedula", "Estado", "Placa"])?;

    insert(&mut table, "id", "1", &[("Cedula", "111"), ("Estado", "A"), ("Placa", "AAA111")]);
    insert(&mut table, "id", "2", &[("Cedula", "111"), ("Estado", "I"), ("Placa", "BBB222")]);
    insert(&mut table, "id", "3", &[("Cedula", "222"), ("Estado", "A"), ("Placa", "CCC333")]);
    table.flush_active_segment()?;

    let fields = vec![
        "id".to_string(),
        "Cedula".to_string(),
        "Estado".to_string(),
        "Placa".to_string(),
    ];
    let filters = vec![eq("Cedula", "111"), eq("Estado", "A")];

    let probe = table
        .probe_fetch_rows(&fields, &filters)?
        .expect("probe path should apply");
    let search = table.search(
        &fields,
        &filters,
        &LogicalOp::And,
        &[],
        &[],
        100,
        0,
        None,
    )?;

    assert_eq!(probe.len(), 1);
    assert_eq!(search.total_found, 1);
    assert_eq!(probe[0][2], "A");
    assert_eq!(probe[0][3], "AAA111");
    Ok(())
}

#[test]
fn probe_fetch_empty_when_no_match() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut table = open_table(dir.path(), "veh", "id", &["id", "Cedula", "Estado"])?;
    insert(&mut table, "id", "1", &[("Cedula", "111"), ("Estado", "I")]);
    table.flush_active_segment()?;

    let fields = vec!["id".to_string(), "Cedula".to_string(), "Estado".to_string()];
    let filters = vec![eq("Cedula", "111"), eq("Estado", "A")];

    let probe = table.probe_fetch_rows(&fields, &filters)?;
    let search = table.search(
        &fields,
        &filters,
        &LogicalOp::And,
        &[],
        &[],
        100,
        0,
        None,
    )?;
    assert_eq!(search.total_found, 0);
    if let Some(rows) = probe {
        assert!(rows.is_empty());
    }
    Ok(())
}

#[test]
fn warm_up_indices_loads_without_prefetch_mmaps() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut table = open_table(dir.path(), "t", "id", &["id", "Score"])?;
    for i in 0..50 {
        insert(&mut table, "id", &i.to_string(), &[("Score", &i.to_string())]);
    }
    table.flush_active_segment()?;
    table.warm_up_indices(&["Score".to_string()])?;

    let filters = vec![eq("Score", "25")];
    let probe = table
        .probe_fetch_rows(&["Score".to_string()], &filters)?
        .expect("probe after warm indices");
    assert_eq!(probe.len(), 1);
    assert_eq!(probe[0][0], "25");
    Ok(())
}
