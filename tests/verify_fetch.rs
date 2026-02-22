use std::collections::HashMap;
use bittice::core::storage::table::Table;
use bittice::core::types::{ComparisonOp, Filter, LogicalOp};

#[test]
fn test_vectorized_fetch() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path().to_path_buf();
    
    let mut table = Table::open(&base_path, "test_vec").unwrap();
    
    // Insert 150 rows
    for i in 0..150 {
        let mut row = HashMap::new();
        row.insert("id".to_string(), i.to_string());
        row.insert("val".to_string(), format!("value_{}", i));
        row.insert("extra".to_string(), "foo".to_string());
        row.insert("field4".to_string(), "bar".to_string());
        table.insert(row).unwrap();
    }
    
    // Flush to ensure data is on disk and stable in immutable segment
    table.flush_active_segment().unwrap();
    
    // Case 1: Fetch all (Vectorized)
    // 150 > 100, so Vectorized should be active.
    let fields = vec!["id".to_string(), "val".to_string(), "extra".to_string(), "field4".to_string()];
    let result = table.search(
        &fields,
        &[], // No filters
        &LogicalOp::And,
        &[], // No aggregations
        &[], // No sorting
        1000,
        0
    ).unwrap();
    
    assert_eq!(result.total_found, 150);
    assert_eq!(result.rows.len(), 150);
    
    let debug_info = result.debug_info.as_ref().unwrap();
    println!("Debug Info: {}", debug_info);
    assert!(debug_info.contains("FetchMode: Vectorized"));
    assert!(debug_info.contains("Chunks: 1"));

    // Verify data
    // Order might be arbitrary if no sort? 
    // Actually, `search` with no sort: `segment_matches` order -> `final_ids`.
    // Since we have 1 segment, it should be insertion order (local_id 0..149).
    // Let's check a few rows.
    let row_0 = result.rows.iter().find(|r| r[0] == "0").unwrap();
    assert_eq!(row_0[1], "value_0");
    
    // Case 2: Fetch few (Row based)
    // We need total_found <= 100 AND fields.len() < 4.
    // But we have 150 rows.
    // If we filter to get fewer results.
    let filter = Filter {
        field: "id".to_string(),
        op: ComparisonOp::Eq,
        value: "0".to_string(),
        value_options: vec![],
    };
    
    // We also need fewer fields to trigger Row mode logic: `total_found > 100 || fields.len() >= 4`
    // If total_found is small (1), we pass first check.
    // But if fields.len() >= 4, we use Vectorized.
    // So we need fields.len() < 4.
    let few_fields = vec!["id".to_string(), "val".to_string()];
    
    let result_small = table.search(
        &few_fields,
        &[filter],
        &LogicalOp::And,
        &[],
        &[],
        10,
        0
    ).unwrap();
    
    assert_eq!(result_small.total_found, 1);
    let debug_info_small = result_small.debug_info.as_ref().unwrap();
    println!("Debug Info Small: {}", debug_info_small);
    assert!(debug_info_small.contains("FetchMode: Row"));
    assert!(debug_info_small.contains("Chunks: 0")); // Initialized to 0
}
