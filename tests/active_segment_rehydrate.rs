//! Active-segment bitmap rehydrate on restart.
//!
//! The index-lag bug: rows inserted into an active segment that never gets
//! rotated have their `value→row-set` bitmaps stored only in memory. A
//! crash before rotation persists `.dat`/`.offsets` (durable via flush
//! buffers) but loses the in-memory bitmap state. On restart, filtered
//! queries (WHERE/JOIN) against the active segment skip those rows even
//! though the data is physically on disk — the JOIN reports N-K rows when
//! the table actually has N.
//!
//! These tests pin the new behavior:
//!   1. Bitmaps persisted on `flush_active_segment_buffers` → next open
//!      reads them straight from disk (happy path, no scan).
//!   2. If the bitmap file is missing (truly crashed before persist),
//!      `SegmentWriter::new` rebuilds the bitmap by scanning `.dat`/
//!      `.offsets`. Queries still return the full row set.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

fn open_test_table(dir: &Path, table_name: &str, pk_field: &str) -> Result<Table> {
    // Persist the manifest BEFORE the first insert so reopen sees
    // primary_key + original_fields set — this matches how a real bittice
    // CDC bootstrap leaves the manifest after the first WriteRowsEvent.
    // Without this the second open() of the same table dir starts with
    // empty original_fields and the insert idempotency path can't read
    // existing rows back to compare.
    let manifest_path = dir.join(table_name).join("manifest.json");
    if manifest_path.exists() {
        // Reopen path: manifest already on disk with the right shape.
        return Table::open(dir, table_name);
    }
    let mut table = Table::open(dir, table_name)?;
    table.manifest.primary_key = pk_field.to_string();
    table.manifest.original_fields = vec![
        pk_field.to_string(),
        "deployment_id".to_string(),
        "hours".to_string(),
    ];
    // Persist so the reopen path can read it.
    table.set_original_fields(table.manifest.original_fields.clone())?;
    Ok(table)
}

fn row(pk: &str, dep_id: &str, hours: &str) -> HashMap<String, String> {
    let mut r = HashMap::new();
    r.insert("id".to_string(), pk.to_string());
    r.insert("deployment_id".to_string(), dep_id.to_string());
    r.insert("hours".to_string(), hours.to_string());
    r
}

fn filter_dep(dep_id: &str) -> Vec<Filter> {
    vec![Filter {
        field: "deployment_id".to_string(),
        op: ComparisonOp::Eq,
        value: dep_id.to_string(),
        value_to: None,
        field_type: None,
        value_options: vec![],
    }]
}

fn count_by_filter(table: &Table, dep_id: &str) -> usize {
    let fields: Vec<String> = vec![
        "id".to_string(),
        "deployment_id".to_string(),
        "hours".to_string(),
    ];
    table
        .search(
            &fields,
            &filter_dep(dep_id),
            &LogicalOp::And,
            &[],
            &[],
            10_000,
            0,
            None,
        )
        .expect("search")
        .total_found
    }

#[test]
fn filtered_query_after_restart_with_persisted_bitmaps() {
    // Happy path: flush_active_segment_buffers persisted bitmaps → reopen
    // reads them from disk. No scan-from-data needed.
    let dir = TempDir::new().expect("tmpdir");

    {
        let mut table = open_test_table(dir.path(), "t", "id").expect("open");
        for i in 0..66 {
            table.insert(row(&i.to_string(), "1", "10")).expect("insert");
        }
        // Buffer flush WITHOUT rotation — this is the per-event CDC flush.
        // After today's fix it MUST also persist bitmaps.
        table.flush_active_segment_buffers().expect("flush buffers");
        // Pretend the process exits here (no close, no rotate).
        table.discard();
    }

    {
        let table = open_test_table(dir.path(), "t", "id").expect("reopen");
        let n = count_by_filter(&table, "1");
        assert_eq!(n, 66, "filtered query must find all 66 rows after restart");
    }
}

#[test]
fn filtered_query_after_restart_with_rebuilt_bitmaps() {
    // Worst-case path: bitmap file is deliberately deleted to simulate a
    // crash that landed between dat/offsets fsync and bitmap persist. The
    // rehydrate code in SegmentWriter::new must rebuild from raw data.
    let dir = TempDir::new().expect("tmpdir");

    let segments_path = {
        let mut table = open_test_table(dir.path(), "t", "id").expect("open");
        for i in 0..66 {
            table.insert(row(&i.to_string(), "1", "10")).expect("insert");
        }
        table.flush_active_segment_buffers().expect("flush buffers");
        let p = dir.path().join("t").join("segments");
        table.discard();
        p
    };

    // Wipe every bitmaps_*.dat in the segment dir to simulate a crash
    // before the bitmap persist landed.
    for entry in std::fs::read_dir(&segments_path).unwrap().flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            for f in std::fs::read_dir(entry.path()).unwrap().flatten() {
                let name = f.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("bitmaps_") && name.ends_with(".dat") {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    }

    {
        let table = open_test_table(dir.path(), "t", "id").expect("reopen");
        let n = count_by_filter(&table, "1");
        assert_eq!(
            n, 66,
            "filtered query must find all 66 rows after rebuild-from-data"
        );
    }
}

#[test]
fn rehydrate_respects_tombstones() {
    // Rows in deleted_bitmap must NOT be re-indexed during rehydrate —
    // otherwise tombstoned rows reappear as live after restart.
    let dir = TempDir::new().expect("tmpdir");

    let segments_path = {
        let mut table = open_test_table(dir.path(), "t", "id").expect("open");
        for i in 0..10 {
            table.insert(row(&i.to_string(), "1", "10")).expect("insert");
        }
        // Delete half — these become tombstones in deleted.bitmap.
        for i in 0..5 {
            table.delete(&i.to_string()).expect("delete");
        }
        table.flush_active_segment_buffers().expect("flush buffers");
        let p = dir.path().join("t").join("segments");
        table.discard();
        p
    };

    // Wipe bitmap files to force rebuild-from-data path.
    for entry in std::fs::read_dir(&segments_path).unwrap().flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            for f in std::fs::read_dir(entry.path()).unwrap().flatten() {
                let name = f.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("bitmaps_") && name.ends_with(".dat") {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    }

    {
        let table = open_test_table(dir.path(), "t", "id").expect("reopen");
        let n = count_by_filter(&table, "1");
        assert_eq!(n, 5, "tombstoned rows must stay deleted across rehydrate");
    }
}
