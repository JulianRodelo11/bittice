mod common;

use bittice::core::storage::table::Table;

fn create_table_with_data(dir: &std::path::Path, name: &str, ds: &common::BenchDataset) {
    let mut table = Table::open(dir, name).expect("open");
    table.manifest.primary_key = "PK".to_string();
    table.manifest.original_fields = vec![
        "PK".to_string(), "Name".to_string(), "Value".to_string(),
        "Category".to_string(), "Status".to_string(), "Description".to_string(),
    ];
    for row in &ds.rows {
        table.insert(row.clone()).expect("insert");
    }
    table.flush_active_segment().expect("flush");
    table.close().expect("close");
}

fn main() {
    let shapes = common::pk_shapes();
    let sizes = common::table_sizes_with_large();

    println!("=== RAM usage (generalization matrix) ===\n");
    println!(
        "{:>12} × {:<18} | {:>10} | {:>10} | {:>10} | {:>8}",
        "rows", "pk_shape", "idx file", "v2 est.", "v1 est.", "ratio"
    );
    println!("{}", "-".repeat(90));

    for &(size_name, row_count) in &sizes {
        for &(pk_name, pk_size) in &shapes {
            let ds = common::generate(pk_size, row_count);
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("bench_ram");

            create_table_with_data(&dir, "t", &ds);
            let idx_path = dir.join("t").join("primary.idx");

            let table = Table::open(&dir, "t").expect("open");
            let idx_len = table.primary_index.len();
            let idx_file_size = std::fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);

            // v2: u128 (16B) + value (12B) = 28B per entry, ×1.5 hashbrown overhead
            let v2_est = (idx_len as u64) * 28 * 3 / 2;
            // v1: pk_size + 24 (String header) + 12 (value) + 40 (bucket overhead) per entry
            let v1_est = (idx_len as u64) * (pk_size as u64 + 24 + 12 + 40);

            let ratio = if v2_est > 0 { v1_est as f64 / v2_est as f64 } else { 0.0 };

            println!(
                "{:>12} × {:<18} | {:>7.1} MB | {:>7.1} MB | {:>7.1} MB | {:>6.1}x",
                size_name, pk_name,
                idx_file_size as f64 / 1_048_576.0,
                v2_est as f64 / 1_048_576.0,
                v1_est as f64 / 1_048_576.0,
                ratio,
            );

            drop(table);
        }
        println!();
    }

    // Analytical model table
    println!("=== Analytical model: reduction ratio ===\n");
    println!("Ratio = (pk_size + 76) / 42  (String header + value + bucket overhead / u128 + value + bucket overhead)");
    println!();
    println!(
        "{:>12} | {:>8} | {:>10}",
        "PK size", "Ratio", "Benefit"
    );
    println!("{}", "-".repeat(40));
    for &(name, size) in &[
        ("8B", 8), ("10B", 10), ("20B", 20), ("36B", 36), ("64B", 64),
        ("100B", 100), ("200B", 200), ("500B", 500), ("1000B", 1000),
    ] {
        let ratio = (size as f64 + 76.0) / 42.0;
        let benefit = if ratio < 1.5 { "minimal" }
            else if ratio < 2.5 { "moderate" }
            else if ratio < 5.0 { "significant" }
            else if ratio < 10.0 { "very significant" }
            else { "dominant" };
        println!("{:>12} | {:>7.1}x | {:>10}", name, ratio, benefit);
    }
}
