mod common;

use std::collections::HashMap;
use std::time::Instant;
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

fn downgrade_to_v1(idx_path: &std::path::Path, ds: &common::BenchDataset) {
    let legacy: HashMap<String, (u64, u32)> = ds.pks.iter().enumerate()
        .map(|(i, pk)| (pk.clone(), (0, i as u32)))
        .collect();
    use std::io::Write;
    let mut file = std::io::BufWriter::new(std::fs::File::create(idx_path).unwrap());
    file.write_all(b"BTPI").unwrap();
    file.write_all(&[1, 0, 0, 0]).unwrap();
    bincode::serialize_into(&mut file, &legacy).unwrap();
    file.flush().unwrap();
}

fn bench_open(dir: &std::path::Path, name: &str, iterations: usize) -> (f64, f64, f64, f64) {
    let mut times_ms = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let _table = Table::open(dir, name).expect("open");
        times_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = *times_ms.first().unwrap();
    let median = times_ms[times_ms.len() / 2];
    let p99_idx = (times_ms.len() as f64 * 0.99) as usize;
    let p99 = times_ms[p99_idx.min(times_ms.len() - 1)];
    let max = *times_ms.last().unwrap();
    (min, median, p99, max)
}

fn main() {
    let iterations = 10;
    let shapes = common::pk_shapes();
    let sizes = common::table_sizes_with_large();

    println!("=== Table::open benchmark (generalization matrix) ===\n");

    for &(size_name, row_count) in &sizes {
        for &(pk_name, pk_size) in &shapes {
            let ds = common::generate(pk_size, row_count);
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("bench_open");

            create_table_with_data(&dir, "t", &ds);
            let idx_path = dir.join("t").join("primary.idx");

            let (v2_min, v2_med, v2_p99, v2_max) = bench_open(&dir, "t", iterations);

            downgrade_to_v1(&idx_path, &ds);
            let (v1_min, v1_med, v1_p99, v1_max) = bench_open(&dir, "t", iterations);

            let idx_size = std::fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);

            println!(
                "{:>12} × {:<18} | idx={:>8.1}KB | v2: min={:>6.1}ms med={:>6.1}ms p99={:>6.1}ms | v1: min={:>6.1}ms med={:>6.1}ms p99={:>7.1}ms",
                size_name, pk_name,
                idx_size as f64 / 1024.0,
                v2_min, v2_med, v2_p99,
                v1_min, v1_med, v1_p99,
            );
        }
        println!();
    }
}
