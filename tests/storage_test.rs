use bittice::core::storage::table::Table;
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
    
    // 4. Reopen Table
    let _table2 = Table::open(base_path, "users").expect("Failed to reopen table");
    // Verify it loaded the segment from manifest (private fields, can't check easily without getters, 
    // but successful open implies manifest read)
}
