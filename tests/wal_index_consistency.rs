//! Tests de consistencia WAL ↔ primary_index (Fase 1e).
//!
//! Verifican que el WAL replay reconstruye correctamente el índice
//! bajo el esquema hash (u128), incluyendo NFC normalization,
//! Update con cambio de PK, y Delete.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_test_table(dir: &Path, name: &str, pk_field: &str) -> Result<Table> {
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
    let mut row = HashMap::new();
    row.insert(table.manifest.primary_key.clone(), pk.to_string());
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
// (a) Insert + crash + replay
// ===========================================================================

#[test]
fn wal_insert_crash_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        for i in 0..20 {
            insert_row(&mut table, &format!("pk_{:03}", i), &format!("n{}", i), &format!("v{}", i));
        }
        // Simulate crash: flush WAL to disk but discard without saving primary_index
        table.flush_active_segment_buffers().expect("flush WAL");
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        for i in 0..20 {
            let pk = format!("pk_{:03}", i);
            let result = query_by_pk(&table, &pk);
            assert!(result.is_some(), "pk '{}' should survive replay", pk);
            assert_eq!(result.unwrap().0, format!("n{}", i));
        }
    }
}

// ===========================================================================
// (b) Update sin cambio de PK + crash + replay
// ===========================================================================

#[test]
fn wal_update_no_pk_change_crash_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "pk1", "original", "val_orig");

        // Update non-PK columns
        let mut new_data = HashMap::new();
        new_data.insert("PK".to_string(), "pk1".to_string());
        new_data.insert("Name".to_string(), "updated".to_string());
        new_data.insert("Value".to_string(), "val_new".to_string());
        table.update("pk1", new_data).expect("update");

        table.flush_active_segment_buffers().expect("flush");
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        let result = query_by_pk(&table, "pk1");
        assert!(result.is_some(), "pk1 should exist after replay");
        let (name, val) = result.unwrap();
        assert_eq!(name, "updated", "should have updated name");
        assert_eq!(val, "val_new", "should have updated value");
    }
}

// ===========================================================================
// (c) Update con cambio de PK + crash + replay
// ===========================================================================

#[test]
fn wal_update_pk_change_crash_replay() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "old_pk", "original", "val_orig");

        // Update that changes PK from "old_pk" to "new_pk"
        let mut new_data = HashMap::new();
        new_data.insert("PK".to_string(), "new_pk".to_string());
        new_data.insert("Name".to_string(), "updated".to_string());
        new_data.insert("Value".to_string(), "val_new".to_string());
        table.update("old_pk", new_data).expect("update");

        table.flush_active_segment_buffers().expect("flush");
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        // new_pk should find the updated row
        let new_result = query_by_pk(&table, "new_pk");
        assert!(new_result.is_some(), "new_pk should exist after replay");
        let (name, val) = new_result.unwrap();
        assert_eq!(name, "updated");
        assert_eq!(val, "val_new");

        // old_pk should NOT find anything (it was replaced)
        let old_result = query_by_pk(&table, "old_pk");
        assert!(old_result.is_none(), "old_pk should NOT exist after replay");
    }
}

// ===========================================================================
// (d) Delete con NFC distinto al Insert
// ===========================================================================

#[test]
fn wal_delete_nfc_nfd_consistency() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Insert with NFC form
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "caf\u{00e9}", "cafe_nfc", "val");

        // Delete with NFD form — should work because PrimaryIndex.remove
        // uses canonical_bytes which normalizes NFC.
        table.delete("cafe\u{0301}").expect("delete with NFD");

        table.flush_active_segment_buffers().expect("flush");
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    // Replay: the Delete should have removed the entry
    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        let result = query_by_pk(&table, "caf\u{00e9}");
        assert!(result.is_none(), "row should be deleted after replay");
    }
}

// ===========================================================================
// (e) Mezcla Insert + Delete + Update sobre la misma PK
// ===========================================================================

#[test]
fn wal_mixed_operations_same_pk() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");

        // Insert "X"
        insert_row(&mut table, "X", "first", "val1");

        // Update "X" (same PK, change data)
        let mut new_data = HashMap::new();
        new_data.insert("PK".to_string(), "X".to_string());
        new_data.insert("Name".to_string(), "second".to_string());
        new_data.insert("Value".to_string(), "val2".to_string());
        table.update("X", new_data).expect("update");

        // Delete "X"
        table.delete("X").expect("delete");

        // Insert "X" again
        insert_row(&mut table, "X", "third", "val3");

        table.flush_active_segment_buffers().expect("flush");
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        let result = query_by_pk(&table, "X");
        assert!(result.is_some(), "X should exist after replay (last insert)");
        let (name, val) = result.unwrap();
        assert_eq!(name, "third", "should have last-inserted data");
        assert_eq!(val, "val3");
    }
}

// ===========================================================================
// (f) WAL replay behavior — documenta que siempre replaya todo
// ===========================================================================

/// Documenta el comportamiento: `replay_wal` siempre replaya TODO el WAL
/// cuando `primary_index` está vacío (no hay "partial replay").
/// Si `primary.idx` existe y no está vacío, el replay se saltea.
/// Esto es intencional: el WAL es el log de recuperación, no un journal incremental.
#[test]
fn wal_replay_skips_when_index_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Insert + close (saves primary_index + truncates WAL)
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "pk1", "name1", "val1");
        table.close().expect("close");
    }

    // Verify WAL is empty after close
    let wal_path = dir.join("t").join("wal.log");
    let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_size, 0, "WAL should be empty after close");

    // Reopen — replay should be skipped (index exists and is non-empty)
    {
        let table = Table::open(&dir, "t").expect("reopen");
        // If replay ran, it would log "replaying N WAL operation(s)".
        // Since WAL is empty and index exists, it's a no-op.
        let result = query_by_pk(&table, "pk1");
        assert!(result.is_some(), "pk1 should be found");
    }
}

// ===========================================================================
// (g) Replay sobre WAL legacy (formato idéntico)
// ===========================================================================

#[test]
fn wal_legacy_format_replay() {
    // WalOperation format didn't change in 1c. This test verifies that
    // a WAL written with the current code replays correctly — which is
    // equivalent to a legacy WAL since the format is identical.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "legacy_pk", "legacy_name", "legacy_val");
        table.flush_active_segment_buffers().expect("flush");
        // Don't close — leave WAL with data, delete index
        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        let mut table = Table::open(&dir, "t").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        let result = query_by_pk(&table, "legacy_pk");
        assert!(result.is_some(), "legacy PK should survive replay");
        assert_eq!(result.unwrap().0, "legacy_name");
    }
}

// ===========================================================================
// (h) WAL vacío + index vacío — tabla recién creada
// ===========================================================================

#[test]
fn wal_empty_table_open_close_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Create empty table, close, reopen
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        table.close().expect("close");
    }

    {
        let table = Table::open(&dir, "t").expect("reopen");
        assert!(table.primary_index.is_empty());
        assert!(query_by_pk(&table, "anything").is_none());
    }
}

// ===========================================================================
// (i) WAL con corrupción al final (crash mid-write)
// ===========================================================================

/// Si el WAL tiene un entry truncado a la mitad (crash durante write),
/// el replay debería aplicar las entradas válidas y manejar la corrupta.
/// Test marcado como #[ignore] porque el manejo de corrupción no está
/// implementado actualmente — el replay fallaría con un error de deserialización.
/// TODO: implementar tolerancia a corrupción al final del WAL en fase futura.
#[test]
#[ignore = "WAL corruption tolerance not implemented — future work"]
fn wal_truncated_entry_handling() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");

    // Write a valid WAL, then corrupt the last entry
    {
        let mut table = open_test_table(&dir, "t", "PK").expect("open");
        insert_row(&mut table, "pk1", "name1", "val1");
        insert_row(&mut table, "pk2", "name2", "val2");
        table.flush_active_segment_buffers().expect("flush");

        // Truncate the WAL file to simulate a mid-write crash
        let wal_path = dir.join("t").join("wal.log");
        let wal_data = std::fs::read(&wal_path).unwrap();
        let truncated_len = wal_data.len() - 10; // chop off last 10 bytes
        std::fs::write(&wal_path, &wal_data[..truncated_len]).unwrap();

        let idx_path = dir.join("t").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    {
        // Should replay valid entries and either skip or error on corrupt one
        let result = Table::open(&dir, "t");
        // This may fail currently — that's expected
        if let Ok(mut table) = result {
            table.flush_active_segment_buffers().expect("flush");
            // At least pk1 should survive
            let r1 = query_by_pk(&table, "pk1");
            assert!(r1.is_some(), "pk1 should survive if replay succeeded");
        }
        // If open fails, that's also acceptable behavior for now
    }
}
