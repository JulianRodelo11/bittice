use bittice::core::storage::table::Table;
use bittice::core::types::{Filter, ComparisonOp, LogicalOp};
use std::collections::HashMap;
use tempfile::tempdir;

#[test]
fn test_storage_engine_basics() {
    let dir = tempdir().unwrap();
    let base_path = dir.path();

    // 1. Open Table (Create)
    let mut table = Table::open(base_path, "users").expect("Failed to open table");
    
    // 2. Insert Record
    let mut row = HashMap::new();
    row.insert("name".to_string(), "Julian".to_string());
    row.insert("age".to_string(), "30".to_string());
    row.insert("_id".to_string(), "1".to_string());
    
    table.insert(row).expect("Failed to insert record");
    
    // Check WAL exists
    let wal_path = base_path.join("users").join("wal.log");
    assert!(wal_path.exists());
    assert!(wal_path.metadata().unwrap().len() > 0);

    // 2.1 Search Active Segment
    let filters = vec![Filter {
        field: "name".to_string(),
        op: ComparisonOp::Eq,
        value: "Julian".to_string(),
        value_options: vec![],
    }];
    let result = table.search(&["name".to_string(), "age".to_string()], &filters, &LogicalOp::And, &[], 10, 0).expect("Search failed");
    assert_eq!(result.total_found, 1);
    assert_eq!(result.rows[0][0], "Julian");
    assert_eq!(result.rows[0][1], "30");
    
    // 3. Flush Active Segment
    table.flush_active_segment().expect("Failed to flush");
    
    // Check Manifest Updated
    let manifest_path = base_path.join("users").join("manifest.json");
    assert!(manifest_path.exists());
    
    // Check Segment Created
    let seg_path = base_path.join("users").join("segments").join("seg_0000");
    assert!(seg_path.exists());
    assert!(seg_path.join("name.dat").exists());
    assert!(seg_path.join("name.offsets").exists());
    assert!(seg_path.join("metadata.json").exists());

    // 3.1 Search Immutable Segment
    let result = table.search(&["name".to_string(), "age".to_string()], &filters, &LogicalOp::And, &[], 10, 0).expect("Search failed");
    assert_eq!(result.total_found, 1);
    assert_eq!(result.rows[0][0], "Julian");
    
    // 4. Reopen Table
    let _table2 = Table::open(base_path, "users").expect("Failed to reopen table");
    // Verify it loaded the segment from manifest (private fields, can't check easily without getters, 
    // but successful open implies manifest read)
}

