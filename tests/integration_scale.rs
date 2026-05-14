//! Integration scale tests (Fase 1f).
//!
//! Validates the storage engine under realistic load patterns.
//! Each scenario exercises multiple components end-to-end.
//!
//! Configuration via env vars:
//!   BITTICE_SCALE_ROWS=N     — rows per scenario (default: 100000 for (a), proportional for others)
//!   BITTICE_RUN_SLOW_TESTS=1 — run tests marked #[ignore]
//!
//! Run with: `cargo test --release --test integration_scale -- --nocapture`

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use bittice::core::storage::primary_index_io;
use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn scale_rows(base: usize) -> usize {
    std::env::var("BITTICE_SCALE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(base)
}

fn run_slow_tests() -> bool {
    std::env::var("BITTICE_RUN_SLOW_TESTS")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_pk(i: usize) -> String {
    // ~200 bytes PK: padded numeric + filler
    format!("{:08}_fill_{:0>180}", i, "x")
}

fn open_table(dir: &Path, name: &str) -> Table {
    let mut table = Table::open(dir, name).expect("open");
    table.manifest.primary_key = "PK".to_string();
    table.manifest.original_fields =
        vec!["PK".to_string(), "Name".to_string(), "Value".to_string()];
    table
}

fn insert_row(table: &mut Table, pk: &str, name: &str, value: &str) {
    let mut row = HashMap::new();
    row.insert("PK".to_string(), pk.to_string());
    row.insert("Name".to_string(), name.to_string());
    row.insert("Value".to_string(), value.to_string());
    table.insert(row).expect("insert");
}

fn query_pk(table: &Table, pk: &str) -> Option<(String, String)> {
    let fields = vec![
        "PK".to_string(),
        "Name".to_string(),
        "Value".to_string(),
    ];
    let filters = vec![Filter {
        field: "PK".to_string(),
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

fn idx_size(dir: &Path, table_name: &str) -> u64 {
    std::fs::metadata(dir.join(table_name).join("primary.idx"))
        .map(|m| m.len())
        .unwrap_or(0)
}

// ===========================================================================
// (a) Volume insert + exhaustive lookup
// ===========================================================================

#[test]
fn scale_volume_insert_lookup() {
    let n = scale_rows(100_000);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    // Insert N rows
    {
        let mut table = open_table(&dir, "t_vol");
        for i in 0..n {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("name_{}", i), &format!("val_{}", i));
            if (i + 1) % 50_000 == 0 {
                table.flush_active_segment().expect("flush");
            }
        }
        table.close().expect("close");
    }

    let insert_ms = start.elapsed().as_millis();

    // Reopen and exhaustive lookup
    let lookup_start = Instant::now();
    {
        let table = Table::open(&dir, "t_vol").expect("reopen");

        // Verify all N rows
        for i in 0..n {
            let pk = make_pk(i);
            let result = query_pk(&table, &pk);
            assert!(result.is_some(), "row {} should be found", i);
            let (name, val) = result.unwrap();
            assert_eq!(name, format!("name_{}", i));
            assert_eq!(val, format!("val_{}", i));
        }

        // Verify non-existent PKs return None
        for i in n..(n + 100) {
            let pk = make_pk(i);
            assert!(query_pk(&table, &pk).is_none(), "row {} should NOT exist", i);
        }
    }

    let lookup_ms = lookup_start.elapsed().as_millis();
    let size = idx_size(&dir, "t_vol");

    println!(
        "[scale_volume] rows={}, insert={}ms, lookup={}ms, primary.idx={} bytes (~{} bytes/entry)",
        n,
        insert_ms,
        lookup_ms,
        size,
        if n > 0 { size / n as u64 } else { 0 }
    );
}

// ===========================================================================
// (b) Compactation with alive/dead mix
// ===========================================================================

#[test]
fn scale_compact_alive_dead_mix() {
    let n = scale_rows(50_000);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    let delete_count = n * 30 / 100;
    let update_count = n * 20 / 100;
    let expected_alive_total = n - delete_count;

    // Insert N rows
    {
        let mut table = open_table(&dir, "t_comp");
        for i in 0..n {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("name_{}", i), &format!("val_{}", i));
        }
        // Flush to create segments for compaction
        table.flush_active_segment().expect("flush");
        // Insert more rows and flush to get >= 4 segments
        for batch in 0..4 {
            for j in 0..100 {
                let pk = make_pk(n + batch * 100 + j);
                insert_row(&mut table, &pk, &format!("extra_{}_{}", batch, j), "extra");
            }
            table.flush_active_segment().expect("flush");
        }
        table.close().expect("close");
    }

    // Delete 30% + Update 20%
    {
        let mut table = open_table(&dir, "t_comp");

        for i in 0..delete_count {
            table.delete(&make_pk(i)).expect("delete");
        }

        for i in delete_count..(delete_count + update_count) {
            let pk = make_pk(i);
            let mut row = HashMap::new();
            row.insert("PK".to_string(), pk.clone());
            row.insert("Name".to_string(), format!("updated_{}", i));
            row.insert("Value".to_string(), format!("newval_{}", i));
            table.update(&pk, row).expect("update");
        }

        table.close().expect("close");
    }

    // Reopen + compact
    {
        let mut table = open_table(&dir, "t_comp");

        let _ = table.compact().expect("compact");

        // Verify deleted rows are gone
        for i in 0..delete_count {
            assert!(
                query_pk(&table, &make_pk(i)).is_none(),
                "deleted row {} should be gone",
                i
            );
        }

        // Verify updated rows have new data
        for i in delete_count..(delete_count + update_count) {
            let result = query_pk(&table, &make_pk(i));
            assert!(result.is_some(), "updated row {} should exist", i);
            let (name, _) = result.unwrap();
            assert_eq!(name, format!("updated_{}", i));
        }

        // Verify alive rows (updated + untouched)
        let mut alive_count = 0usize;
        for i in delete_count..n {
            let result = query_pk(&table, &make_pk(i));
            assert!(result.is_some(), "alive row {} should exist", i);
            alive_count += 1;
        }

        assert_eq!(
            alive_count,
            expected_alive_total,
            "alive count mismatch: expected {}, got {}",
            expected_alive_total,
            alive_count
        );
    }

    let elapsed = start.elapsed().as_millis();
    println!(
        "[scale_compact] n={}, deleted={}, updated={}, alive={}, time={}ms",
        n, delete_count, update_count, expected_alive_total, elapsed
    );
}

// ===========================================================================
// (c) Eviction + reopen under pressure
// ===========================================================================

#[test]
fn scale_eviction_reopen_pressure() {
    if !run_slow_tests() {
        eprintln!("skipping scale_eviction_reopen_pressure — set BITTICE_RUN_SLOW_TESTS=1");
        return;
    }

    let n_tables = 10;
    let rows_per_table = scale_rows(10_000);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    std::env::set_var("BITTICE_MAX_OPEN_TABLES", "3");

    let tm = bittice::server::table_manager::TableManager::new();

    // Create tables and insert data
    for t in 0..n_tables {
        let table_name = format!("t_{}", t);
        let table_lock = tm.get_table("ent", &table_name).expect("get_table");
        let mut tbl = table_lock.write().unwrap();
        tbl.manifest.primary_key = "PK".to_string();
        tbl.manifest.original_fields =
            vec!["PK".to_string(), "Name".to_string(), "Value".to_string()];
        for i in 0..rows_per_table {
            let pk = make_pk(i);
            insert_row(&mut tbl, &pk, &format!("name_{}_{}", t, i), &format!("val_{}_{}", t, i));
        }
        tbl.flush_active_segment_buffers().expect("flush");
    }

    // Round-robin queries — forces evictions and reopens
    let mut total_queries = 0usize;
    let mut failures = 0usize;
    for round in 0..3 {
        for t in 0..n_tables {
            let table_name = format!("t_{}", t);
            let table_lock = tm.get_table("ent", &table_name).expect("get_table");
            let tbl = table_lock.read().unwrap();

            // Sample 100 random PKs
            for i in (0..rows_per_table).step_by(rows_per_table / 100).take(100) {
                let pk = make_pk(i);
                let result = query_pk(&tbl, &pk);
                if result.is_none() {
                    failures += 1;
                    eprintln!(
                        "FAIL: round={}, table={}, pk={}, expected to find row",
                        round, t, i
                    );
                }
                total_queries += 1;
            }
        }
    }

    assert_eq!(failures, 0, "{} queries failed", failures);

    let elapsed = start.elapsed().as_millis();
    println!(
        "[scale_eviction] tables={}, rows/table={}, queries={}, failures={}, time={}ms",
        n_tables, rows_per_table, total_queries, failures, elapsed
    );

    std::env::remove_var("BITTICE_MAX_OPEN_TABLES");
}

// ===========================================================================
// (d) Crash recovery with large WAL
// ===========================================================================

#[test]
fn scale_crash_recovery_large_wal() {
    let n = scale_rows(50_000);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    // Insert N rows, flush WAL but delete primary.idx (simulates crash)
    {
        let mut table = open_table(&dir, "t_crash");
        for i in 0..n {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("name_{}", i), &format!("val_{}", i));
        }
        // Flush WAL to disk so replay can work
        table.flush_active_segment_buffers().expect("flush WAL");
        // Delete primary.idx to force replay on next open
        let idx_path = dir.join("t_crash").join("primary.idx");
        let _ = std::fs::remove_file(&idx_path);
        table.discard();
    }

    // Reopen — WAL replay should rebuild index
    let replay_start = Instant::now();
    {
        let mut table = Table::open(&dir, "t_crash").expect("reopen");
        table.flush_active_segment_buffers().expect("flush after replay");

        // Verify all rows
        let mut found = 0usize;
        for i in 0..n {
            let pk = make_pk(i);
            if let Some((name, val)) = query_pk(&table, &pk) {
                assert_eq!(name, format!("name_{}", i));
                assert_eq!(val, format!("val_{}", i));
                found += 1;
            }
        }
        assert_eq!(found, n, "expected {} rows after replay, found {}", n, found);
    }

    let replay_ms = replay_start.elapsed().as_millis();
    let total_ms = start.elapsed().as_millis();
    let size = idx_size(&dir, "t_crash");

    println!(
        "[scale_crash_recovery] rows={}, replay={}ms, total={}ms, primary.idx={} bytes",
        n, replay_ms, total_ms, size
    );
}

// ===========================================================================
// (e) Mixed operations with oracle verification
// ===========================================================================

#[test]
fn scale_mixed_operations_oracle() {
    let n_init = scale_rows(20_000);
    let n_ops = 10_000;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    // Oracle: in-memory model of expected state
    let mut oracle: HashMap<String, (String, String)> = HashMap::new();

    // Insert initial rows
    {
        let mut table = open_table(&dir, "t_mix");
        for i in 0..n_init {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("name_{}", i), &format!("val_{}", i));
            oracle.insert(pk, (format!("name_{}", i), format!("val_{}", i)));
        }
        table.flush_active_segment_buffers().expect("flush");
    }

    // Mixed operations
    let mut table = open_table(&dir, "t_mix");

    let mut insert_counter = n_init;
    let mut stats = (0usize, 0usize, 0usize, 0usize); // insert, query, update, delete

    for op_idx in 0..n_ops {
        let op_type = op_idx % 10;
        match op_type {
            // 40% Insert
            0..=3 => {
                let pk = make_pk(insert_counter);
                let name = format!("new_{}", insert_counter);
                let val = format!("newval_{}", insert_counter);
                insert_row(&mut table, &pk, &name, &val);
                oracle.insert(pk, (name, val));
                insert_counter += 1;
                stats.0 += 1;
            }
            // 30% Query
            4..=6 => {
                if !oracle.is_empty() {
                    // Flush to make recently-inserted data visible via mmap
                    table.flush_active_segment_buffers().expect("flush before query");
                    let keys: Vec<_> = oracle.keys().take(1).cloned().collect();
                    if let Some(pk) = keys.first() {
                        let result = query_pk(&table, pk);
                        if let Some(expected) = oracle.get(pk) {
                            if let Some((name, val)) = result {
                                assert_eq!(
                                    (&name, &val),
                                    (&expected.0, &expected.1),
                                    "query mismatch for pk={}",
                                    pk
                                );
                            } else {
                                panic!("query returned None for existing pk={}", pk);
                            }
                        }
                    }
                }
                stats.1 += 1;
            }
            // 20% Update (no PK change)
            7..=8 => {
                if !oracle.is_empty() {
                    let keys: Vec<_> = oracle.keys().take(1).cloned().collect();
                    if let Some(pk) = keys.first() {
                        let pk = pk.clone();
                        let new_name = format!("upd_{}", op_idx);
                        let new_val = format!("updval_{}", op_idx);
                        let mut row = HashMap::new();
                        row.insert("PK".to_string(), pk.clone());
                        row.insert("Name".to_string(), new_name.clone());
                        row.insert("Value".to_string(), new_val.clone());
                        table.update(&pk, row).expect("update");
                        oracle.insert(pk, (new_name, new_val));
                        stats.2 += 1;
                    }
                }
            }
            // 10% Delete
            9 => {
                if oracle.len() > 100 {
                    // Keep at least 100 rows
                    let keys: Vec<_> = oracle.keys().take(1).cloned().collect();
                    if let Some(pk) = keys.first() {
                        let pk = pk.clone();
                        table.delete(&pk).expect("delete");
                        oracle.remove(&pk);
                        stats.3 += 1;
                    }
                }
            }
            _ => unreachable!(),
        }

        // Periodic flush
        if (op_idx + 1) % 5000 == 0 {
            table.flush_active_segment_buffers().expect("flush");
        }
    }

    table.close().expect("close");

    // Final verification: reopen and compare against oracle
    let verify_start = Instant::now();
    {
        let mut table = Table::open(&dir, "t_mix").expect("reopen final");
        table.manifest.primary_key = "PK".to_string();
        table.manifest.original_fields =
            vec!["PK".to_string(), "Name".to_string(), "Value".to_string()];
        table.flush_active_segment_buffers().expect("flush before verify");

        let mut engine_count = 0usize;
        for (pk, (expected_name, expected_val)) in &oracle {
            let result = query_pk(&table, pk);
            assert!(
                result.is_some(),
                "engine missing pk={} that oracle has",
                pk
            );
            let (name, val) = result.unwrap();
            assert_eq!(
                (&name, &val),
                (expected_name, expected_val),
                "data mismatch for pk={}",
                pk
            );
            engine_count += 1;
        }

        assert_eq!(
            engine_count,
            oracle.len(),
            "engine has {} rows, oracle has {}",
            engine_count,
            oracle.len()
        );
    }

    let verify_ms = verify_start.elapsed().as_millis();
    let total_ms = start.elapsed().as_millis();

    println!(
        "[scale_mixed] init={}, ops={}, inserts={}, queries={}, updates={}, deletes={}, oracle_size={}, verify={}ms, total={}ms",
        n_init, n_ops, stats.0, stats.1, stats.2, stats.3, oracle.len(), verify_ms, total_ms
    );
}

// ===========================================================================
// (f) Migration CLI on realistic dataset
// ===========================================================================

#[test]
fn scale_migration_realistic_dataset() {
    let n = scale_rows(50_000);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    // Create table with current code (writes v2)
    {
        let mut table = open_table(&dir, "t_mig");
        for i in 0..n {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("name_{}", i), &format!("val_{}", i));
        }
        table.close().expect("close");
    }

    let idx_path = dir.join("t_mig").join("primary.idx");

    // Verify it's v2
    let raw = std::fs::read(&idx_path).unwrap();
    assert_eq!(&raw[..4], b"BTPI");
    assert_eq!(raw[4], 2);

    // Downgrade to v1: build a synthetic v1 index from scratch using the same data.
    // This simulates a pre-1c index file.
    {
        // Build v1 directly from the rows we know
        let legacy: HashMap<String, (u64, u32)> = (0..n)
            .map(|i| (make_pk(i), (0, i as u32)))
            .collect();

        // Write v1 file
        {
            use std::io::Write;
            let mut file = std::io::BufWriter::new(std::fs::File::create(&idx_path).unwrap());
            file.write_all(b"BTPI").unwrap();
            file.write_all(&[1, 0, 0, 0]).unwrap();
            bincode::serialize_into(&mut file, &legacy).unwrap();
            file.flush().unwrap();
        }
    }

    // Verify it's now v1
    let raw = std::fs::read(&idx_path).unwrap();
    assert_eq!(raw[4], 1);

    // Run migration
    let result = bittice::core::migrate_primary_index::migrate_table(
        "ent", "t_mig", &dir.join("t_mig"), false, true, false,
    );
    assert!(result.error.is_none(), "migration error: {:?}", result.error);
    assert_eq!(result.entries, n);
    assert_eq!(result.collisions, 0);

    // Verify v2
    let raw = std::fs::read(&idx_path).unwrap();
    assert_eq!(raw[4], 2);

    // Verify backup
    let backup_path = dir.join("t_mig").join("primary.idx.v1.bak");
    assert!(backup_path.exists());

    let elapsed = start.elapsed().as_millis();
    let size = idx_size(&dir, "t_mig");
    println!(
        "[scale_migration] rows={}, collisions={}, primary.idx={} bytes, time={}ms",
        n, result.collisions, size, elapsed
    );
}

// ===========================================================================
// (g) Sustained insert throughput sanity check
// ===========================================================================

#[test]
#[ignore = "sustained 1M insert test — run with BITTICE_RUN_SLOW_TESTS=1"]
fn scale_sustained_insert_1m() {
    let n = 1_000_000;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ent");
    let start = Instant::now();

    {
        let mut table = open_table(&dir, "t_1m");
        for i in 0..n {
            let pk = make_pk(i);
            insert_row(&mut table, &pk, &format!("n{}", i), &format!("v{}", i));
            if (i + 1) % 100_000 == 0 {
                table.flush_active_segment().expect("flush");
                println!("  inserted {}/{} rows ({}ms)", i + 1, n, start.elapsed().as_millis());
            }
        }
        table.close().expect("close");
    }

    let elapsed = start.elapsed().as_millis();
    let size = idx_size(&dir, "t_1m");
    println!(
        "[scale_1m] rows={}, time={}ms, primary.idx={} bytes (~{} bytes/entry)",
        n,
        elapsed,
        size,
        if n > 0 { size / n as u64 } else { 0 }
    );
}
